// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `sqlite_master` decode + minimal DDL reader. Lives outside
//! `src/parser/` by design (spec 002-parser Requirement 5): zero
//! dependency on the full SQL parser, so schema decoding keeps working
//! with the parser feature-gated off. `src/parser/` now exists (and
//! already has a `CREATE INDEX` AST/parser), but this module deliberately
//! does not call into it — see #211.
//!
//! Unparseable DDL (e.g. a virtual table) degrades to raw-row access
//! (`columns: vec![]`), never an error — Tier 0's "graceful unknowns"
//! (spec 001-architecture Requirement 4). The same rule extends to
//! `CREATE INDEX` entries (#211): an index this reader can't find a
//! column list for (e.g. an auto-index, whose `sqlite_master.sql` is
//! `NULL`) is silently omitted from `TableSchema::indexes`.
//!
//! The DDL parser here is deliberately naive, matching spike 005 (#12)'s
//! prototype: no quoted/bracketed identifiers with embedded whitespace,
//! no dialect edge cases beyond what the real corpus fixtures exercise.
//! "Nothing more" than table name, column names, declared types (via
//! column name extraction only — types are not separately captured),
//! WITHOUT ROWID / STRICT markers, and (#211) each table's indexes
//! (column list, ASC/DESC, uniqueness), per the originating issues' scope.

use crate::storage::row::btree::{BtreeError, TableCursor};
use crate::storage::row::record::{decode_record, Collation, RecordError, TextEncoding, Value};

/// The schema catalog types are defined once in db-core (`crate::schema`,
/// db-core ADR 0014) and re-exported here, exactly as [`Value`]/[`Collation`]
/// are (db-core ADR 0010): a schema this reader returns is the very type
/// db-core's row codegen consumes, so no consumer converts.
pub use crate::schema::{IndexSchema, IndexedColumn, TableSchema, ViewSchema};
use crate::storage::row::vfs::PageSource;

/// Failure walking or decoding `sqlite_master`.
#[derive(Debug)]
pub enum DdlError {
    /// Failure walking the `sqlite_master` b-tree.
    Btree(BtreeError),

    /// Failure decoding a `sqlite_master` row's record payload.
    Record(RecordError),

    /// A `sqlite_master` row didn't have exactly 5 columns.
    MalformedRow(usize),
}

impl std::fmt::Display for DdlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DdlError::Btree(e) => write!(f, "walking sqlite_master: {e}"),
            DdlError::Record(e) => write!(f, "decoding a sqlite_master row: {e}"),
            DdlError::MalformedRow(n) => {
                write!(f, "sqlite_master row has {n} columns, expected 5")
            }
        }
    }
}

impl std::error::Error for DdlError {}

impl From<BtreeError> for DdlError {
    fn from(e: BtreeError) -> Self {
        DdlError::Btree(e)
    }
}

impl From<RecordError> for DdlError {
    fn from(e: RecordError) -> Self {
        DdlError::Record(e)
    }
}

/// Walks `sqlite_master` via `cursor` (which callers MUST construct with
/// `root_page = 1`) and returns every `type = 'table'` entry as a
/// [`TableSchema`]. Never errors on unparseable or virtual-table DDL —
/// those degrade to `columns: vec![]`.
pub fn read_schema<P: PageSource>(
    cursor: &mut TableCursor<P>,
    encoding: TextEncoding,
) -> Result<Vec<TableSchema>, DdlError> {
    read_schema_and_views(cursor, encoding).map(|(schemas, _)| schemas)
}

/// Walks `sqlite_master` once and returns both the `type = 'table'`
/// entries (as [`read_schema`] would) and the `type = 'view'` entries
/// (as [`read_views`] would). Callers that need both catalogs —
/// `compile_statement` takes both — should use this instead of two
/// separate b-tree walks (#589).
pub fn read_schema_and_views<P: PageSource>(
    cursor: &mut TableCursor<P>,
    encoding: TextEncoding,
) -> Result<(Vec<TableSchema>, Vec<ViewSchema>), DdlError> {
    let mut schemas: Vec<TableSchema> = Vec::new();
    let mut views = Vec::new();
    // Table name → position in `schemas`, so index attachment below is
    // a hash lookup instead of an O(indexes × tables) linear scan (#589).
    let mut table_pos: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut pending_indexes: Vec<(String, IndexSchema)> = Vec::new();
    let mut row = cursor.first_row()?;
    while let Some(r) = row {
        let values = decode_record(&r.payload, encoding)?;
        if values.len() != 5 {
            return Err(DdlError::MalformedRow(values.len()));
        }
        match text(values.first()) {
            "table" => {
                let schema = table_schema(&values);
                table_pos.insert(schema.name.clone(), schemas.len());
                schemas.push(schema);
            }
            "index" => pending_indexes.extend(index_schema(&values)),
            "view" => views.push(ViewSchema {
                name: text(values.get(1)).to_string(),
                sql: text(values.get(4)).to_string(),
            }),
            _ => {}
        }
        row = cursor.next_row()?;
    }
    for (table_name, index) in pending_indexes {
        if let Some(schema) = table_pos
            .get(&table_name)
            .and_then(|&pos| schemas.get_mut(pos))
        {
            schema.indexes.push(index);
        }
    }
    Ok((schemas, views))
}

/// Walks `sqlite_master` via `cursor` (root_page = 1) and returns every
/// `type IN ('table', 'view')` name, unfiltered and in storage order —
/// the shell's `.tables` needs both kinds of names but none of
/// [`TableSchema`]'s DDL parsing, so this bypasses [`read_schema`]
/// entirely rather than teaching it a schema-kind discriminator.
pub fn read_table_and_view_names<P: PageSource>(
    cursor: &mut TableCursor<P>,
    encoding: TextEncoding,
) -> Result<Vec<String>, DdlError> {
    let mut names = Vec::new();
    let mut row = cursor.first_row()?;
    while let Some(r) = row {
        let values = decode_record(&r.payload, encoding)?;
        if values.len() != 5 {
            return Err(DdlError::MalformedRow(values.len()));
        }
        if matches!(text(values.first()), "table" | "view") {
            names.push(text(values.get(1)).to_string());
        }
        row = cursor.next_row()?;
    }
    Ok(names)
}

/// Walks `sqlite_master` via `cursor` (root_page = 1) and returns every
/// `type = 'view'` entry as a [`ViewSchema`] — the catalog counterpart
/// to [`read_schema`]'s tables, used by `codegen::expand_views` to
/// resolve a `FROM`-clause name against the view catalog before
/// falling back to a real table.
pub fn read_views<P: PageSource>(
    cursor: &mut TableCursor<P>,
    encoding: TextEncoding,
) -> Result<Vec<ViewSchema>, DdlError> {
    let mut views = Vec::new();
    let mut row = cursor.first_row()?;
    while let Some(r) = row {
        let values = decode_record(&r.payload, encoding)?;
        if values.len() != 5 {
            return Err(DdlError::MalformedRow(values.len()));
        }
        if text(values.first()) == "view" {
            views.push(ViewSchema {
                name: text(values.get(1)).to_string(),
                sql: text(values.get(4)).to_string(),
            });
        }
        row = cursor.next_row()?;
    }
    Ok(views)
}

fn table_schema(values: &[Value]) -> TableSchema {
    let name = text(values.get(1)).to_string();
    let root_page = match values.get(3) {
        // A root page outside `u32` is not a page; 0 is the existing
        // "no root" value the callers already handle.
        Some(Value::Integer(i)) => u32::try_from(*i).unwrap_or(0),
        _ => 0,
    };
    let sql = text(values.get(4));

    if is_virtual_table(sql) {
        return TableSchema {
            name,
            root_page: 0,
            columns: Vec::new(),
            without_rowid: false,
            strict: false,
            column_types: Vec::new(),
            column_collations: Vec::new(),
            is_virtual: true,
            sql: sql.to_string(),
            indexes: Vec::new(),
            rowid_alias: None,
        };
    }

    let parsed = parse_create_table(sql).unwrap_or_default();
    TableSchema {
        name,
        root_page,
        columns: parsed.columns,
        without_rowid: parsed.without_rowid,
        strict: parsed.strict,
        column_types: parsed.column_types,
        column_collations: parsed.column_collations,
        is_virtual: false,
        sql: sql.to_string(),
        indexes: Vec::new(),
        rowid_alias: None,
    }
    .with_computed_rowid_alias()
}

/// Parses a `sqlite_master` row with `type = 'index'` into the owning
/// table's name and a naive [`IndexSchema`]. Returns `None` for anything
/// this naive reader can't find a column list in — most notably
/// auto-indexes (`sql` is `NULL` in `sqlite_master`) — treated identically
/// to unparseable table DDL: graceful, never an error.
fn index_schema(values: &[Value]) -> Option<(String, IndexSchema)> {
    let name = text(values.get(1)).to_string();
    let table_name = text(values.get(2)).to_string();
    let root_page = match values.get(3) {
        // A root page outside `u32` is not a page; 0 is the existing
        // "no root" value the callers already handle.
        Some(Value::Integer(i)) => u32::try_from(*i).unwrap_or(0),
        _ => 0,
    };
    let sql = text(values.get(4));

    let (start, end) = column_list_span(sql)?;
    let inner = sql.get(start..end)?;
    let columns = split_top_level_commas(inner)
        .into_iter()
        .map(indexed_column)
        .collect();

    Some((
        table_name,
        IndexSchema {
            name,
            unique: is_unique_index(sql),
            columns,
            root_page,
        },
    ))
}

/// Whether `s` starts with `prefix`, ASCII-case-insensitively, without
/// allocating an uppercased copy (#589). `prefix` must be ASCII (all
/// callers pass SQL keywords), so slicing `s` at `prefix.len()` either
/// lands on a char boundary or `get` returns `None` — never a panic.
fn starts_with_ignore_case(s: &str, prefix: &str) -> bool {
    s.get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

/// If `s` ends with `suffix` ASCII-case-insensitively, the byte index
/// where the suffix starts (i.e. `s.get(..idx)` strips it) —
/// allocation-free (#589), same ASCII-`suffix` char-boundary reasoning
/// as [`starts_with_ignore_case`].
fn suffix_start_ignore_case(s: &str, suffix: &str) -> Option<usize> {
    let split = s.len().checked_sub(suffix.len())?;
    s.get(split..)?
        .eq_ignore_ascii_case(suffix)
        .then_some(split)
}

/// Byte offset of the first ASCII-case-insensitive occurrence of
/// `needle` in `haystack`, allocation-free (#589) — the caller-facing
/// contract matches `haystack.to_ascii_uppercase().find(needle)` for an
/// ASCII `needle`.
fn find_ignore_case(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    let end = haystack.len().checked_sub(needle.len())?;
    (0..=end).find(|&i| {
        haystack
            .get(i..i.saturating_add(needle.len()))
            .is_some_and(|window| window.eq_ignore_ascii_case(needle))
    })
}

fn is_unique_index(sql: &str) -> bool {
    starts_with_ignore_case(sql.trim_start(), "CREATE UNIQUE INDEX")
}

/// Splits a single indexed-column definition (`col`, `col DESC`, `col
/// COLLATE NOCASE ASC`, ...) into its column name and sort direction.
/// Expression indexes (`lower(col)`) are kept as raw text in `name` —
/// this reader doesn't parse expressions, matching the rest of this
/// module's "nothing more than the corpus needs" scope.
fn indexed_column(def: &str) -> IndexedColumn {
    let mut span = def.trim();
    let mut desc = false;
    if let Some(split) = suffix_start_ignore_case(span, " DESC") {
        desc = true;
        span = span.get(..split).unwrap_or(span).trim_end();
    } else if let Some(split) = suffix_start_ignore_case(span, " ASC") {
        span = span.get(..split).unwrap_or(span).trim_end();
    }

    let collation = column_collation(span);

    if let Some(pos) = find_ignore_case(span, " COLLATE ") {
        span = span.get(..pos).unwrap_or(span).trim_end();
    }

    IndexedColumn {
        name: span
            .trim_matches(['"', '`', '['].as_ref())
            .trim_matches([']'].as_ref())
            .to_string(),
        desc,
        collation,
    }
}

/// The [`Collation`] declared by a `COLLATE name` constraint anywhere in
/// `def` (a column definition or indexed-column spec), or
/// [`Collation::Binary`] when absent or unrecognized — matching SQLite's
/// own BINARY default.
fn column_collation(def: &str) -> Collation {
    let Some(pos) = find_ignore_case(def, "COLLATE") else {
        return Collation::Binary;
    };
    let rest = def
        .get(pos.saturating_add("COLLATE".len())..)
        .unwrap_or("")
        .trim_start();
    let name = rest
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(['"', '`', '['].as_ref())
        .trim_matches([']'].as_ref());
    if name.eq_ignore_ascii_case("NOCASE") {
        Collation::NoCase
    } else if name.eq_ignore_ascii_case("RTRIM") {
        Collation::RTrim
    } else {
        Collation::Binary
    }
}

fn text(v: Option<&Value>) -> &str {
    match v {
        Some(Value::Text(s)) => s,
        _ => "",
    }
}

fn is_virtual_table(sql: &str) -> bool {
    starts_with_ignore_case(sql.trim_start(), "CREATE VIRTUAL TABLE")
}

#[derive(Default)]
struct ParsedCreateTable {
    columns: Vec<String>,
    column_types: Vec<String>,
    column_collations: Vec<Collation>,
    without_rowid: bool,
    strict: bool,
}

/// Parses `CREATE TABLE ... (col-defs) [table-options]`. Returns `None`
/// for anything this naive reader can't find a column list in — the
/// caller treats that identically to a virtual table (empty columns,
/// never an error).
fn parse_create_table(sql: &str) -> Option<ParsedCreateTable> {
    let (start, end) = column_list_span(sql)?;
    let inner = sql.get(start..end)?;
    let trailer = sql.get(end.saturating_add(1)..)?.to_ascii_uppercase();

    let defs: Vec<&str> = split_top_level_commas(inner)
        .into_iter()
        .filter(|def| !is_table_constraint(def))
        .collect();
    let columns = defs.iter().map(|def| column_name(def)).collect();
    let column_types = defs.iter().map(|def| column_type(def)).collect();
    let column_collations = defs.iter().map(|def| column_collation(def)).collect();

    Some(ParsedCreateTable {
        columns,
        column_types,
        column_collations,
        without_rowid: trailer.contains("WITHOUT ROWID"),
        strict: trailer.contains("STRICT"),
    })
}

/// A standalone table-level constraint (`PRIMARY KEY(...)`, `UNIQUE(...)`,
/// `FOREIGN KEY(...)`, `CHECK(...)`, `CONSTRAINT name ...`) rather than a
/// column definition. Inline column-level constraints (e.g. `k PRIMARY
/// KEY`) don't start with the keyword — the column name comes first —
/// so this check doesn't false-positive on those.
fn is_table_constraint(def: &str) -> bool {
    let trimmed = def.trim();
    starts_with_ignore_case(trimmed, "PRIMARY KEY")
        || starts_with_ignore_case(trimmed, "UNIQUE")
        || starts_with_ignore_case(trimmed, "FOREIGN KEY")
        || starts_with_ignore_case(trimmed, "CHECK")
        || starts_with_ignore_case(trimmed, "CONSTRAINT")
}

/// Replaces the contents of quoted regions (`'...'`, `"..."`,
/// `` `...` ``, `[...]`) and comments (`--...`, `/*...*/`) with spaces,
/// preserving every other byte and the overall byte length — so a
/// masked offset always lines up with the same offset in the original
/// string. Callers that need to find top-level structure (parens,
/// commas, keywords) scan the masked bytes but slice the *original*
/// string for the text they return, so quoted content survives intact
/// while it can no longer be mistaken for syntax (#135: a comma or the
/// words PRIMARY/KEY inside a string literal used to be indistinguishable
/// from the real thing).
fn mask_quotes_and_comments(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = bytes.to_vec();
    let mut i = 0usize;
    let at = |i: usize| bytes.get(i).copied();
    let blank = |out: &mut [u8], i: usize| {
        if let Some(b) = out.get_mut(i) {
            *b = b' ';
        }
    };
    while let Some(here) = at(i) {
        match here {
            b'\'' | b'"' => {
                let quote = here;
                blank(&mut out, i);
                i = i.saturating_add(1);
                while let Some(b) = at(i) {
                    if b == quote {
                        blank(&mut out, i);
                        i = i.saturating_add(1);
                        // A doubled quote (`''`/`""`) is an escaped
                        // literal quote character, not the closing one.
                        if at(i) == Some(quote) {
                            blank(&mut out, i);
                            i = i.saturating_add(1);
                            continue;
                        }
                        break;
                    }
                    blank(&mut out, i);
                    i = i.saturating_add(1);
                }
            }
            b'`' | b'[' => {
                let close = if here == b'`' { b'`' } else { b']' };
                blank(&mut out, i);
                i = i.saturating_add(1);
                while let Some(b) = at(i) {
                    blank(&mut out, i);
                    i = i.saturating_add(1);
                    if b == close {
                        break;
                    }
                }
            }
            b'-' if at(i.saturating_add(1)) == Some(b'-') => {
                while let Some(b) = at(i) {
                    if b == b'\n' {
                        break;
                    }
                    blank(&mut out, i);
                    i = i.saturating_add(1);
                }
            }
            b'/' if at(i.saturating_add(1)) == Some(b'*') => {
                blank(&mut out, i);
                blank(&mut out, i.saturating_add(1));
                i = i.saturating_add(2);
                while let Some(b) = at(i) {
                    if b == b'*' && at(i.saturating_add(1)) == Some(b'/') {
                        blank(&mut out, i);
                        blank(&mut out, i.saturating_add(1));
                        i = i.saturating_add(2);
                        break;
                    }
                    blank(&mut out, i);
                    i = i.saturating_add(1);
                }
            }
            _ => i = i.saturating_add(1),
        }
    }
    out
}

/// Byte range *between* the outer parens of a `CREATE TABLE`'s
/// column-definition list, or `None` when there is no balanced list.
/// Paren depth is tracked over the quote/comment-masked text (#135) so a
/// paren inside a string literal can't unbalance the scan, but the
/// returned range indexes the original `sql` string.
fn column_list_span(sql: &str) -> Option<(usize, usize)> {
    let masked = mask_quotes_and_comments(sql);
    let start = masked.iter().position(|&b| b == b'(')?;
    let mut depth = 0i32;
    for (offset, &b) in masked.get(start..)?.iter().enumerate() {
        match b {
            b'(' => depth = depth.saturating_add(1),
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some((start.saturating_add(1), start.saturating_add(offset)));
                }
            }
            _ => {}
        }
    }
    None
}

/// The single top-level-comma splitter behind both
/// [`parse_create_table`]'s `columns` and [`column_defs`]. These two
/// MUST agree position-for-position: [`TableSchema::with_computed_rowid_alias`] returns an
/// index into `column_defs`, and `src/codegen/expr.rs` resolves that
/// index against `TableSchema::columns`. Two copies of this loop would
/// let the lists drift and silently mis-target the rowid substitution,
/// so they share one implementation rather than mirroring each other.
/// Splits over the quote/comment-masked text (#135) so a comma inside a
/// string literal (e.g. `DEFAULT 'a,b'`) is not mistaken for a column
/// separator; the returned slices still come from the original `inner`.
fn split_top_level_commas(inner: &str) -> Vec<&str> {
    let masked = mask_quotes_and_comments(inner);
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut part_start = 0usize;
    for (i, &b) in masked.iter().enumerate() {
        match b {
            b'(' => depth = depth.saturating_add(1),
            b')' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                parts.push(inner.get(part_start..i).unwrap_or("").trim());
                part_start = i.saturating_add(1);
            }
            _ => {}
        }
    }
    parts.push(inner.get(part_start..).unwrap_or("").trim());
    parts
}

fn column_name(def: &str) -> String {
    def.split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(['"', '`', '['].as_ref())
        .trim_matches([']'].as_ref())
        .to_string()
}

/// The declared-type text between a column's name and its first
/// column-constraint keyword — SQLite's own notion of "declared type"
/// (datatype3.html §3.1), which may span several words (`DOUBLE
/// PRECISION`, `UNSIGNED BIG INT`). Empty when the column has no type
/// (`CREATE TABLE t(x)`).
pub fn column_type(def: &str) -> String {
    const CONSTRAINT_KEYWORDS: [&str; 8] = [
        "PRIMARY",
        "NOT",
        "NULL",
        "UNIQUE",
        "CHECK",
        "DEFAULT",
        "COLLATE",
        "REFERENCES",
    ];
    def.split_whitespace()
        .skip(1)
        .take_while(|word| {
            !CONSTRAINT_KEYWORDS.iter().any(|kw| {
                word.eq_ignore_ascii_case(kw)
                    || (starts_with_ignore_case(word, kw)
                        && word.as_bytes().get(kw.len()) == Some(&b'('))
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Splits a `CREATE TABLE ...(col-defs)...` statement's column-definition
/// list into raw per-column definition strings, in declared order —
/// re-derived from `schema.sql` rather than kept alongside `columns`,
/// which holds names only — `src/dump.rs` needs each column's declared
/// type text, and [`TableSchema::with_computed_rowid_alias`] needs its full constraint
/// text. Shares [`split_top_level_commas`] and [`is_table_constraint`]
/// with [`parse_create_table`], so the two column lists cannot drift.
pub fn column_defs(schema: &TableSchema) -> Vec<&str> {
    all_defs(schema)
        .into_iter()
        .filter(|def| !is_table_constraint(def))
        .collect()
}

/// Every top-level def in the column list, column definitions and
/// table-level constraints alike — the raw material [`column_defs`]
/// filters down and [`TableSchema::with_computed_rowid_alias`] additionally needs the
/// constraint side of (to recognize a table-level `PRIMARY KEY(col)`).
fn all_defs(schema: &TableSchema) -> Vec<&str> {
    let Some((start, end)) = column_list_span(&schema.sql) else {
        return Vec::new();
    };
    let Some(inner) = schema.sql.get(start..end) else {
        return Vec::new();
    };
    split_top_level_commas(inner)
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
    use crate::storage::row::header::DatabaseHeader;
    use crate::storage::row::vfs::{UnixVfs, Vfs, VfsPageSource};
    use std::path::Path;

    fn read_fixture(family: &str, name: &str) -> Vec<TableSchema> {
        let path = Path::new("tests/corpus/fixtures").join(family).join(name);
        let vfs = UnixVfs;
        let file = vfs
            .open_read(&path)
            .unwrap_or_else(|e| panic!("open {path:?}: {e}"));
        let mut header_buf = [0u8; 100];
        file.read_at(&mut header_buf, 0).unwrap();
        let header = DatabaseHeader::parse(&header_buf).unwrap();
        let source = VfsPageSource::open(&vfs, &path, header.page_size).unwrap();
        let mut cursor = TableCursor::new(source, &header, 1);
        read_schema(&mut cursor, header.text_encoding).unwrap()
    }

    fn find(schemas: &[TableSchema], name: &str) -> TableSchema {
        schemas
            .iter()
            .find(|s| s.name == name)
            .cloned()
            .unwrap_or_else(|| panic!("no table named {name} in {schemas:?}"))
    }

    #[test]
    fn plain_table_columns_and_rootpage() {
        let schemas = read_fixture("btrees", "table_single_page.db");
        assert_eq!(schemas.len(), 1);
        let t = find(&schemas, "t");
        assert_eq!(t.columns, vec!["a", "b"]);
        assert_eq!(t.root_page, 2);
        assert!(!t.without_rowid);
        assert!(!t.strict);
        assert!(!t.is_virtual);
    }

    #[test]
    fn without_rowid_marker_detected() {
        let schemas = read_fixture("btrees", "without_rowid.db");
        let t = find(&schemas, "t");
        assert_eq!(t.columns, vec!["k", "v"]);
        assert!(t.without_rowid);
    }

    #[test]
    fn strict_marker_detected_and_generated_column_named_correctly() {
        let schemas = read_fixture("features", "strict_generated.db");
        let t = find(&schemas, "t");
        // `b INTEGER GENERATED ALWAYS AS (a*2) STORED` — the naive
        // column-name extractor must not be confused by the nested
        // parens in the generated-column expression.
        assert_eq!(t.columns, vec!["a", "b"]);
        assert!(t.strict);
        assert!(!t.without_rowid);
    }

    #[test]
    fn autovacuum_fixture_plain_table_unaffected_by_pointer_map_page() {
        let schemas = read_fixture("features", "autovacuum.db");
        let t = find(&schemas, "t");
        assert_eq!(t.columns, vec!["a", "b"]);
    }

    #[test]
    fn fts5_virtual_table_is_graceful_unknown_shadow_tables_are_readable() {
        let schemas = read_fixture("features", "fts5.db");

        let virtual_entry = find(&schemas, "t");
        assert!(virtual_entry.is_virtual);
        assert!(virtual_entry.columns.is_empty());
        assert_eq!(virtual_entry.root_page, 0);

        // Shadow tables are ordinary sqlite_master table entries and
        // must be fully readable, including the WITHOUT ROWID ones
        // (spike 005, #12's finding that motivated spec 006 Req 5/6).
        assert_eq!(find(&schemas, "t_data").columns, vec!["id", "block"]);
        assert!(!find(&schemas, "t_data").without_rowid);

        let t_idx = find(&schemas, "t_idx");
        // `(segid, term, pgno, PRIMARY KEY(segid, term))` — the
        // standalone table-level PRIMARY KEY(...) constraint must not be
        // mistaken for a 4th column named "PRIMARY".
        assert_eq!(t_idx.columns, vec!["segid", "term", "pgno"]);
        assert!(t_idx.without_rowid);

        assert_eq!(find(&schemas, "t_content").columns, vec!["id", "c0"]);
        assert_eq!(find(&schemas, "t_docsize").columns, vec!["id", "sz"]);

        let t_config = find(&schemas, "t_config");
        // `(k PRIMARY KEY, v)` — inline column-level PRIMARY KEY on `k`
        // must still name the column "k", not be filtered out.
        assert_eq!(t_config.columns, vec!["k", "v"]);
        assert!(t_config.without_rowid);
    }

    #[test]
    fn rtree_virtual_table_is_graceful_unknown_shadow_tables_are_readable() {
        let schemas = read_fixture("features", "rtree.db");

        let virtual_entry = find(&schemas, "t");
        assert!(virtual_entry.is_virtual);
        assert!(virtual_entry.columns.is_empty());

        assert_eq!(find(&schemas, "t_rowid").columns, vec!["rowid", "nodeno"]);
        assert_eq!(find(&schemas, "t_node").columns, vec!["nodeno", "data"]);
        assert_eq!(
            find(&schemas, "t_parent").columns,
            vec!["nodeno", "parentnode"]
        );
    }

    #[test]
    fn unparseable_non_virtual_ddl_degrades_gracefully_never_errors() {
        // Synthetic row: no column-list parens at all. Not a real
        // fixture case (every real CREATE TABLE has a paren list), but
        // exercises the "never an error" guarantee for anything this
        // naive reader genuinely can't make sense of.
        let values = vec![
            Value::Text("table".to_string().into()),
            Value::Text("weird".to_string().into()),
            Value::Text("weird".to_string().into()),
            Value::Integer(5),
            Value::Text("garbage with no parens".to_string().into()),
        ];
        let schema = table_schema(&values);
        assert_eq!(schema.name, "weird");
        assert_eq!(schema.root_page, 5);
        assert!(schema.columns.is_empty());
        assert!(!schema.is_virtual);
    }

    #[test]
    fn non_table_entries_are_excluded() {
        // index.db has an explicit secondary index (idx_b) alongside
        // table t — read_schema must return only tables as top-level
        // entries, with idx_b attached to t's `indexes` (#211).
        let schemas = read_fixture("btrees", "index.db");
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].name, "t");
        assert_eq!(schemas[0].indexes.len(), 1);
        assert_eq!(schemas[0].indexes[0].name, "idx_b");
        assert!(!schemas[0].indexes[0].unique);
        assert_eq!(schemas[0].indexes[0].columns.len(), 1);
        assert_eq!(schemas[0].indexes[0].columns[0].name, "b");
        assert!(!schemas[0].indexes[0].columns[0].desc);
    }

    #[test]
    fn unique_index_with_multiple_columns_and_desc() {
        let sql = "CREATE UNIQUE INDEX idx_ab ON t(a DESC, b)";
        let (table_name, index) = index_schema(&[
            Value::Null,
            Value::Text("idx_ab".to_string().into()),
            Value::Text("t".to_string().into()),
            Value::Integer(0),
            Value::Text(sql.to_string().into()),
        ])
        .expect("index_schema should parse a simple CREATE UNIQUE INDEX");
        assert_eq!(table_name, "t");
        assert!(index.unique);
        assert_eq!(index.columns.len(), 2);
        assert_eq!(index.columns[0].name, "a");
        assert!(index.columns[0].desc);
        assert_eq!(index.columns[1].name, "b");
        assert!(!index.columns[1].desc);
    }

    #[test]
    fn auto_index_with_null_sql_is_omitted() {
        // Auto-indexes created for PRIMARY KEY/UNIQUE constraints have a
        // NULL `sql` column in sqlite_master — this naive reader can't
        // find a column list in an empty string, so it's gracefully
        // skipped rather than erroring.
        let result = index_schema(&[
            Value::Null,
            Value::Text("sqlite_autoindex_t_1".to_string().into()),
            Value::Text("t".to_string().into()),
            Value::Integer(0),
            Value::Null,
        ]);
        assert!(result.is_none());
    }
}
