// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Real-index range-seek fast paths (#606, ADR-0034): `col BETWEEN lo AND
//! hi`, `col LIKE 'prefix%'`/`col GLOB 'prefix*'`, and `col IN (v1, ...)`
//! against a single-column-indexed `WHERE` clause each get an
//! index-b-tree seek in place of a full `Rewind`/`Next` scan +
//! per-row filter. Each fast path is narrowly pattern-matched, mirroring
//! `limit_scan.rs`'s `try_compile_rowid_seek`/
//! `try_compile_covering_index_scan` — anything outside the recognized
//! shape returns `Ok(false)` and leaves `em`/`reg` untouched, so the
//! caller falls back to the ordinary scan and the existing `BETWEEN`/
//! `IN`/`LIKE` filter lowering in `src/codegen/expr/cond.rs` is used
//! unchanged.
//!
//! `BETWEEN`/`IN` walk `[lo, hi]`/`{v1, v2, ...}` using the new
//! `SeekIndexGE`/`IdxCompareGT` opcodes (see ADR-0034): `SeekIndexGE`
//! seeks to the range floor, then an `IdxNext` loop guarded by
//! `IdxCompareGT` (checked at the top of each iteration) stops once the
//! walk passes the upper bound.
//!
//! `LIKE 'prefix%'` reuses the same `SeekIndexGE`/`IdxCompareGT` pair
//! rather than a dedicated prefix-compare opcode: the seek floor is the
//! prefix itself, and the upper bound is `prefix` with the maximum
//! Unicode scalar value (`char::MAX`, U+10FFFF) appended. Any string
//! that starts with `prefix` followed by further characters sorts
//! strictly below `prefix + char::MAX` (its first extra character can
//! only be `char::MAX` itself in the same, practically nonexistent,
//! edge case that would also confuse SQLite's own byte-increment
//! upper-bound trick), and `prefix` itself (no suffix, matching `LIKE
//! 'prefix%'` per its trailing `%` matching zero characters) sorts
//! below `prefix + char::MAX` as a strict prefix always sorts before a
//! longer string sharing that prefix. This keeps the upper-bound
//! representation a plain `Value::Text` usable with the same
//! `IdxCompareGT` opcode BETWEEN already needs, rather than a second
//! bespoke byte-prefix-compare primitive.
use super::limit_scan::{compile_limit_setup, emit_limit_guard, emit_offset_guard};
use super::projection::emit_row_via_sink;
use super::*;
use crate::parser::ast::BinaryOp;
use crate::parser::Span;
use crate::vm::row::{affinity_of, Affinity};
use std::rc::Rc;

fn dummy_span() -> Span {
    Span {
        line: 0,
        column: 0,
        offset: 0,
        len: 0,
    }
}

fn literal_expr(lit: Literal) -> Expr {
    Expr {
        kind: ExprKind::Literal(lit),
        span: dummy_span(),
    }
}

pub(super) fn where_col(expr: &Expr) -> Option<&str> {
    match &expr.kind {
        ExprKind::Column { name, .. } => Some(name.as_str()),
        _ => None,
    }
}

/// Same operand restriction as `limit_scan.rs`'s fast paths: a literal
/// (int/float/string — string included here since `BETWEEN`/`IN` over a
/// text column is common, unlike the rowid-seek path's integer-only
/// scope) or a bind parameter. Anything else (a sub-expression, a named
/// parameter) falls back to the ordinary scan.
///
/// Does NOT by itself guarantee the literal is safe to seek with — a
/// seek builds a raw probe key compared byte-for-byte against what's
/// already stored in the index (itself built with the indexed column's
/// declared affinity applied at `INSERT` time), unlike the ordinary
/// filter path's `Eq`/`Ge`/`Le` opcodes, which apply *comparison*
/// affinity dynamically at compare time from both operands' types. A
/// literal whose storage class doesn't already match the column's
/// affinity (e.g. the string `'10'` against an `INTEGER`-affinity
/// column) would silently seek to the wrong place — SQLite's own
/// affinity-coercion rules (well-formed numeric text only) aren't
/// reproduced here. [`operand_matches_column_affinity`] is the
/// additional check every fast path in this file layers on top of this
/// one before trusting an operand.
pub(super) fn is_supported_operand(expr: &Expr) -> bool {
    matches!(
        &expr.kind,
        ExprKind::Literal(Literal::Integer(_) | Literal::Float(_) | Literal::Str(_))
            | ExprKind::Param(ParamKind::Anonymous | ParamKind::Numbered(_))
    )
}

/// #280's widened bound eligibility: anything [`is_supported_operand`]
/// already accepts, plus anything "constant for the duration of the
/// scan" — an uncorrelated scalar subquery (no reference to `scope`'s
/// own table or anything outside its own `FROM`, checked by
/// [`subquery_hoistable`]), or an expression built purely out of such
/// constants (`CAST`, unary `-`/`+`, `COLLATE`, parens, and binary
/// arithmetic). A correlated subquery is *not* loop-constant (its value
/// can differ per outer row, which a seek computed once before the loop
/// can't account for), so it's rejected same as any other unsupported
/// shape — matching this file's existing "return `Ok(false)`, fall back
/// to the ordinary scan" convention for anything it doesn't recognize.
pub(super) fn is_constant_operand(expr: &Expr, scope: &Scope) -> bool {
    match &expr.kind {
        ExprKind::Subquery(subquery) => {
            crate::codegen::row::subquery::subquery_hoistable(subquery, scope)
        }
        ExprKind::Unary { expr: e, .. }
        | ExprKind::Cast { expr: e, .. }
        | ExprKind::Collate { expr: e, .. }
        | ExprKind::Paren(e) => is_constant_operand(e, scope),
        ExprKind::Binary { lhs, rhs, .. } => {
            is_constant_operand(lhs, scope) && is_constant_operand(rhs, scope)
        }
        _ => is_supported_operand(expr),
    }
}

/// Whether `operand`'s literal storage class already matches
/// `column_affinity` closely enough that building a raw seek probe from
/// it — with no affinity coercion applied — compares correctly against
/// what's actually stored in the index. A bind parameter is passed
/// through uncheckable at compile time (same accepted risk every other
/// equality fast path in `limit_scan.rs` already takes for `Param`
/// operands); only a `Literal` operand is actually constrained here.
/// See [`is_supported_operand`]'s doc for why this check exists.
pub(super) fn operand_matches_column_affinity(expr: &Expr, column_affinity: Affinity) -> bool {
    match &expr.kind {
        ExprKind::Literal(Literal::Integer(_) | Literal::Float(_)) => matches!(
            column_affinity,
            Affinity::Integer | Affinity::Real | Affinity::Numeric
        ),
        ExprKind::Literal(Literal::Str(_)) => matches!(column_affinity, Affinity::Text),
        // #280: a bind parameter's storage class is genuinely unknowable
        // here, so it's passed through uncheckable, same accepted risk
        // as `Param` above. A subquery/arithmetic-composed constant
        // operand (only reachable via `is_constant_operand`, never
        // `is_supported_operand` alone) is the same kind of unknowable
        // at compile time -- its runtime value's storage class isn't
        // fixed by its shape the way a literal's is -- so it gets the
        // same pass-through rather than a coercion this file doesn't
        // implement yet (no VM opcode applies column affinity to an
        // already-computed register; see #280's follow-up).
        ExprKind::Param(_)
        | ExprKind::Subquery(_)
        | ExprKind::Unary { .. }
        | ExprKind::Cast { .. }
        | ExprKind::Collate { .. }
        | ExprKind::Paren(_)
        | ExprKind::Binary { .. } => true,
        _ => false,
    }
}

/// [`affinity_of`] for `col_name`'s declared type in `schema`, defaulting
/// to [`Affinity::Blob`] (SQLite's own default for a column with no
/// declared type) if the name can't be resolved — callers only reach
/// this after [`find_leading_index`] already confirmed the column
/// exists, so an unresolved name here would be a schema/index
/// inconsistency, not a normal fallback path; defaulting to `Blob`
/// (never matches a `Literal` operand in
/// [`operand_matches_column_affinity`]) just means such an
/// inconsistency falls back to the ordinary scan rather than panicking.
fn column_affinity(schema: &TableSchema, col_name: &str) -> Affinity {
    schema
        .columns
        .iter()
        .position(|c| c.eq_ignore_ascii_case(col_name))
        .and_then(|i| schema.column_types.get(i))
        .map_or(Affinity::Blob, |ty| affinity_of(ty))
}

/// Finds the position (into `schema.indexes`) of an index whose
/// *leading* column matches `col_name` — mirrors
/// `limit_scan.rs::find_covering_index`'s index lookup. Only the
/// leading column of the match is ever probed/compared by the opcodes
/// these fast paths emit, so a multi-column index still works (just as
/// a leading-column-only lookup). Returns a position rather than a
/// borrowed `&IndexSchema` (an explicit lifetime parameter would be
/// needed to return one, past this codebase's qualified-language-subset
/// limit, `tools/mvl-limit`) — every caller already re-fetches via
/// `schema.indexes.get(position)` right after.
///
/// Only a `BINARY`-collated leading column qualifies (#298): the index
/// b-tree is stored and searched in BINARY order (`storage::row::btree::
/// index`, Tier 0), so a `SeekIndexGE`/`IdxCompareGT` walk keyed on a
/// `NOCASE`/`RTRIM` column would descend to the wrong leaf and stop at the
/// wrong entry -- `WHERE name = 'alice'` on a `NOCASE` index returned zero
/// rows for `Alice`/`alice`/`ALICE`. Such indexes fall back to the
/// ordinary scan (whose `Eq` filter applies the collation), same as an
/// unindexed column would.
pub(super) fn find_leading_index(schema: &TableSchema, col_name: &str) -> Option<usize> {
    schema.indexes.iter().position(|idx| {
        idx.columns.first().is_some_and(|c| {
            c.name.eq_ignore_ascii_case(col_name) && c.collation == Collation::Binary
        })
    })
}

/// Opens `index` on `cursors.sort` (reused across every fast path in
/// this file, mirroring `limit_scan.rs`/`index_scan.rs`'s own reuse of
/// that cursor slot — none of these paths ever run alongside a sort).
fn open_index_cursor(
    em: &mut Emitter,
    index: &IndexSchema,
    index_cursor: i32,
) -> Result<(), CodegenError> {
    let root_page = crate::codegen::row::index_maintenance::valid_index_root_page(index)?;
    let mut open_instr = Instruction::new(Opcode::OpenRead, index_cursor, root_page, 0);
    open_instr.p5 = 1;
    em.emit(open_instr);
    Ok(())
}

/// Every select-list column, when it's a bare unqualified column
/// reference (`ResultColumn::Expr` wrapping `ExprKind::Column{table:
/// None, catalog: None, ..}`) — the shape every fast path in this file
/// is profiled against (`SELECT id, n, x, ... FROM t WHERE ...`), and
/// the only shape [`emit_matched_row`]'s indexed-column substitution
/// (#664) knows how to recognize. `None` for `*`/`tbl.*`, a qualified
/// reference, or any computed expression — [`emit_matched_row`] falls
/// back to the ordinary full-row read via [`emit_row_via_sink`] for
/// those, exactly as before this optimization existed.
fn bare_column_names(select: &Select) -> Option<Vec<&str>> {
    select
        .columns
        .iter()
        .map(|col| match col {
            ResultColumn::Expr {
                expr:
                    Expr {
                        kind:
                            ExprKind::Column {
                                table: None,
                                catalog: None,
                                name,
                            },
                        ..
                    },
                ..
            } => Some(name.as_str()),
            _ => None,
        })
        .collect()
}

/// Reads `idx`'s value from `index_cursor`'s own leading key column
/// (position 0) into `dest`, instead of [`emit_column_read`]'s ordinary
/// read off the table cursor (#664) — every fast path in this file
/// positions its index cursor on an entry whose key already carries the
/// one column the seek matched against, so re-reading that same column
/// a moment later off the table row `IdxRowid`+`SeekRowid` just fetched
/// is a wholly redundant round trip (the table lookup itself still
/// stands: every *other* selected column still only lives in the table
/// row). Mirrors `emit_column_read`'s REAL-affinity fixup (#143) since
/// this is still logically that same schema column, just sourced from a
/// different cursor; the rowid-alias case never arises here — a
/// rowid-alias column is never itself indexed as an ordinary index
/// column, so [`emit_matched_row`] never requests it.
fn emit_indexed_column_read(
    em: &mut Emitter,
    schema: &TableSchema,
    index_cursor: i32,
    idx: usize,
    dest: i32,
) {
    em.emit(Instruction::new(Opcode::Column, index_cursor, 0, dest));
    if schema
        .column_types
        .get(idx)
        .is_some_and(|t| affinity_of(t) == Affinity::Real)
    {
        em.emit(Instruction::new(Opcode::RealAffinity, dest, 0, 0));
    }
}

/// Emits the shared "fetch the full row and hand it to `sink`" tail
/// every fast path in this file uses once the index cursor is
/// positioned on a matching entry: `IdxRowid` + `SeekRowid` (jumping to
/// `row_skip` if the table row is somehow missing), then LIMIT/OFFSET
/// guards, then the row projection. `indexed_col_name` is the column
/// this fast path's seek matched against (already sitting in
/// `index_cursor`'s key) — when every select-list column is a bare
/// reference ([`bare_column_names`]), that one column is read straight
/// from the index cursor instead of the table row (#664); any other
/// select-list shape falls back to [`emit_row_via_sink`]'s ordinary
/// full-row read, unchanged from before this optimization existed.
#[allow(clippy::too_many_arguments)]
fn emit_matched_row<F>(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    select: &Select,
    schema: &Rc<TableSchema>,
    cursors: ScanCursors,
    index_cursor: i32,
    indexed_col_name: &str,
    limit: &Option<super::limit_scan::LimitState>,
    row_skip: Label,
    end_label: Label,
    catalog: &[TableSchema],
    sink: &mut F,
) -> Result<(), CodegenError>
where
    F: FnMut(&mut Emitter, &mut RegAlloc, i32, i32) -> Result<(), CodegenError>,
{
    let rowid_reg = reg.alloc();
    em.emit(Instruction::new(
        Opcode::IdxRowid,
        index_cursor,
        rowid_reg,
        0,
    ));
    let table_seek_addr = em.emit(Instruction::new(
        Opcode::SeekRowid,
        cursors.table,
        0,
        rowid_reg,
    ));
    em.patch_p2(table_seek_addr, row_skip);

    if let Some(limit) = limit {
        emit_offset_guard(em, limit, row_skip);
    }
    if let Some(limit) = limit {
        emit_limit_guard(em, limit, end_label);
    }

    if let Some(names) = bare_column_names(select) {
        let mut first = None;
        for name in &names {
            let idx = column_index(schema, name).ok_or_else(|| CodegenError::UnknownColumn {
                name: (*name).to_string(),
            })?;
            let r = reg.alloc();
            if name.eq_ignore_ascii_case(indexed_col_name) && schema.rowid_alias != Some(idx) {
                emit_indexed_column_read(em, schema, index_cursor, idx, r);
            } else {
                emit_column_read(em, schema, cursors.table, idx, r)?;
            }
            first.get_or_insert(r);
        }
        let first = first.unwrap_or_else(|| reg.alloc());
        return sink(em, reg, first, i32::try_from(names.len()).unwrap_or(0));
    }
    emit_row_via_sink(em, reg, select, schema, cursors.table, false, catalog, sink)
}

/// Compiles `WHERE col BETWEEN lo AND hi` (`col` a plain column with a
/// matching index, `lo`/`hi` literals or bind parameters) as a
/// `SeekIndexGE(lo)` + `IdxCompareGT(hi)`-guarded `IdxNext` walk, in
/// place of the ordinary `Rewind`/`Next` scan + `compile_cond`'s
/// `Ge`/`Le` filter (`src/codegen/expr/cond.rs`, untouched, still used
/// for every other shape). Returns `Ok(false)` — `em`/`reg` untouched —
/// for `NOT BETWEEN`, a non-column `expr`, an unindexed column, an
/// unsupported operand, `DISTINCT`, or any WHERE clause that isn't a
/// single top-level `BETWEEN`.
#[allow(clippy::too_many_arguments)]
pub(super) fn try_compile_between_seek<F>(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    select: &Select,
    schema: &Rc<TableSchema>,
    cursors: ScanCursors,
    end_label: Label,
    catalog: &[TableSchema],
    sink: &mut F,
) -> Result<bool, CodegenError>
where
    F: FnMut(&mut Emitter, &mut RegAlloc, i32, i32) -> Result<(), CodegenError>,
{
    if matches!(select.distinct, Some(Distinctness::Distinct)) {
        return Ok(false);
    }
    let Some(where_expr) = &select.where_clause else {
        return Ok(false);
    };
    let Some((col, lo, hi, _)) = as_bounds(where_expr) else {
        return Ok(false);
    };
    let Some(col_name) = where_col(col) else {
        return Ok(false);
    };
    let scope = Scope::single_shared(schema, cursors.table).with_catalog(catalog);
    if !is_constant_operand(lo, &scope) || !is_constant_operand(hi, &scope) {
        return Ok(false);
    }
    let Some(index_position) = find_leading_index(schema, col_name) else {
        return Ok(false);
    };
    let Some(index) = schema.indexes.get(index_position) else {
        return Ok(false);
    };
    let affinity = column_affinity(schema, col_name);
    if !operand_matches_column_affinity(lo, affinity)
        || !operand_matches_column_affinity(hi, affinity)
    {
        return Ok(false);
    }
    let leading_collation = index
        .columns
        .first()
        .map_or(Collation::Binary, |c| c.collation);

    let index_cursor = cursors.sort;
    open_index_cursor(em, index, index_cursor)?;

    let limit = compile_limit_setup(em, reg, &scope, select)?;
    let lo_reg = compile_value(em, reg, &scope, lo)?;
    // `col = lit` (#298): one bound, evaluated once.
    let hi_reg = if std::ptr::eq(lo, hi) {
        lo_reg
    } else {
        compile_value(em, reg, &scope, hi)?
    };
    emit_bounded_index_walk(
        em,
        index_cursor,
        lo_reg,
        hi_reg,
        leading_collation,
        end_label,
        |em, row_skip| {
            emit_matched_row(
                em,
                reg,
                select,
                schema,
                cursors,
                index_cursor,
                col_name,
                &limit,
                row_skip,
                end_label,
                catalog,
                sink,
            )
        },
    )?;
    Ok(true)
}

/// #666: `UPDATE`/`DELETE`'s row-scan equivalent of
/// [`try_compile_forward_comparison_seek`]/[`try_compile_between_seek`],
/// stripped of `SELECT`-only concerns (`DISTINCT`, LIMIT/OFFSET,
/// projection) — recognizes the same two WHERE shapes (`col >/>=/</<=
/// lit` and `col BETWEEN lo AND hi` against a leading-indexed column)
/// and emits the same `SeekIndexGE`/`IdxCompareGT`/`IdxNext` walk, but
/// calls `sink` once per matching *index* entry instead of fetching the
/// full table row itself — `sink` is responsible for turning that into
/// a table-row action (typically `IdxRowid` + `SeekRowid` onto
/// `row_skip` on a miss, then the caller's own per-row body). Returns
/// `Ok(false)` — `em`/`reg` untouched — for any unrecognized shape,
/// exactly like its `SELECT` counterparts.
/// Which index [`try_compile_range_row_seek`] would walk for `where_expr`
/// (position in `schema.indexes`), or `None` when it returns `Ok(false)`
/// -- the eligibility half of that row seek, shared with
/// [`super::eqp::explain_query_plan`] (#282) so an aggregate/`GROUP BY`
/// plan reports `SEARCH` exactly when #279's seek compiles. Narrower than
/// [`range_seek_index_position`] on purpose: the row seek recognizes only
/// `BETWEEN` and a forward comparison, not `LIKE`-prefix or `IN`.
pub(super) fn range_row_seek_index_position(
    where_expr: &Expr,
    schema: &TableSchema,
    catalog: &[TableSchema],
) -> Option<usize> {
    let recognized = as_bounds(where_expr).is_some() || as_forward_comparison(where_expr).is_some();
    if recognized {
        range_seek_index_position(where_expr, schema, catalog)
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn try_compile_range_row_seek<F>(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    where_expr: &Expr,
    schema: &TableSchema,
    scope: &Scope,
    index_cursor: i32,
    end_label: Label,
    sink: &mut F,
) -> Result<bool, CodegenError>
where
    F: FnMut(&mut Emitter, &mut RegAlloc, i32, Label) -> Result<(), CodegenError>,
{
    if let Some((col, lo, hi, _)) = as_bounds(where_expr) {
        let Some(col_name) = where_col(col) else {
            return Ok(false);
        };
        if !is_constant_operand(lo, scope) || !is_constant_operand(hi, scope) {
            return Ok(false);
        }
        let Some(index_position) = find_leading_index(schema, col_name) else {
            return Ok(false);
        };
        let Some(index) = schema.indexes.get(index_position) else {
            return Ok(false);
        };
        let affinity = column_affinity(schema, col_name);
        if !operand_matches_column_affinity(lo, affinity)
            || !operand_matches_column_affinity(hi, affinity)
        {
            return Ok(false);
        }
        let leading_collation = index
            .columns
            .first()
            .map_or(Collation::Binary, |c| c.collation);
        open_index_cursor(em, index, index_cursor)?;
        let lo_reg = compile_value(em, reg, scope, lo)?;
        let hi_reg = if std::ptr::eq(lo, hi) {
            lo_reg
        } else {
            compile_value(em, reg, scope, hi)?
        };
        emit_bounded_index_walk(
            em,
            index_cursor,
            lo_reg,
            hi_reg,
            leading_collation,
            end_label,
            |em, row_skip| sink(em, reg, index_cursor, row_skip),
        )?;
        return Ok(true);
    }

    let Some((col_name, operand, inclusive)) = as_forward_comparison(where_expr) else {
        return Ok(false);
    };
    if !is_constant_operand(operand, scope) {
        return Ok(false);
    }
    let Some(index_position) = find_leading_index(schema, col_name) else {
        return Ok(false);
    };
    let Some(index) = schema.indexes.get(index_position) else {
        return Ok(false);
    };
    let affinity = column_affinity(schema, col_name);
    if !operand_matches_column_affinity(operand, affinity) {
        return Ok(false);
    }
    let leading_collation = index
        .columns
        .first()
        .map_or(Collation::Binary, |c| c.collation);

    open_index_cursor(em, index, index_cursor)?;
    let bound_reg = compile_value(em, reg, scope, operand)?;

    // #280: see `try_compile_between_seek`'s identical comment.
    let bound_null_addr = em.emit(Instruction::new(Opcode::IsNull, bound_reg, 0, 0));
    em.patch_p2(bound_null_addr, end_label);

    let seek_addr = em.emit(Instruction::with_p4(
        Opcode::SeekIndexGE,
        index_cursor,
        0,
        bound_reg,
        P4::SeekKey(vec![leading_collation]),
    ));
    em.patch_p2(seek_addr, end_label);

    if !inclusive {
        let skip_start = em.new_label();
        em.place(skip_start);
        let past_bound = em.new_label();
        let gt_addr = em.emit(Instruction::with_p4(
            Opcode::IdxCompareGT,
            index_cursor,
            0,
            bound_reg,
            P4::SeekKey(vec![leading_collation]),
        ));
        em.patch_p2(gt_addr, past_bound);
        let skip_next_addr = em.emit(Instruction::new(Opcode::IdxNext, index_cursor, 0, 0));
        em.patch_p2(skip_next_addr, skip_start);
        let exhausted_addr = em.emit(Instruction::new(Opcode::Goto, 0, 0, 0));
        em.patch_p2(exhausted_addr, end_label);
        em.place(past_bound);
    }

    let loop_start = em.new_label();
    em.place(loop_start);
    let row_skip = em.new_label();
    sink(em, reg, index_cursor, row_skip)?;
    em.place(row_skip);
    let next_addr = em.emit(Instruction::new(Opcode::IdxNext, index_cursor, 0, 0));
    em.patch_p2(next_addr, loop_start);
    Ok(true)
}

/// The maximum Unicode scalar value, `char::MAX` (U+10FFFF) — see this
/// module's doc comment for why appending it to a literal prefix gives a
/// safe strict upper bound for `LIKE 'prefix%'`/`GLOB 'prefix*'`.
fn prefix_upper_bound(prefix: &str) -> String {
    let mut s = String::with_capacity(prefix.len().saturating_add(4));
    s.push_str(prefix);
    s.push(char::MAX);
    s
}

/// Extracts the literal, non-wildcard prefix of a `LIKE`/`GLOB` pattern
/// string, requiring exactly one trailing wildcard (`%`/`*`) and nothing
/// else wildcard-ish anywhere — see this module's doc and #606's bail-out
/// list. Returns `None` for any pattern shape outside that.
pub(super) fn like_literal_prefix(pattern: &str, glob: bool) -> Option<String> {
    let wildcard = if glob { '*' } else { '%' };
    let single = if glob { '?' } else { '_' };
    if pattern.is_empty() {
        return None;
    }
    let prefix = pattern.strip_suffix(wildcard)?;
    if prefix.is_empty() {
        // Empty prefix (leading wildcard, or the pattern is just "%")
        // matches everything — no seek floor to compute.
        return None;
    }
    if prefix.contains(wildcard) || prefix.contains(single) {
        return None;
    }
    if !glob && prefix.contains('\\') {
        // Conservative: an ESCAPE clause changes how backslashes (or
        // whatever escape char is named) are interpreted — bail rather
        // than risk misreading an escaped wildcard as literal.
        return None;
    }
    if prefix.ends_with('\u{10FFFF}') {
        // The one prefix value `prefix_upper_bound` can't safely
        // represent an exclusive-enough bound for — bail per #606.
        return None;
    }
    Some(prefix.to_string())
}

/// Compiles `WHERE col LIKE 'prefix%'` / `WHERE col GLOB 'prefix*'`
/// (`col` a plain column with a matching index, pattern a string
/// literal with exactly one non-empty literal prefix followed by a
/// single trailing wildcard) as a `SeekIndexGE`/`IdxCompareGT` range
/// walk — see this module's doc comment for the upper-bound
/// construction. Returns `Ok(false)` for `NOT LIKE`/`NOT GLOB`, an
/// `ESCAPE` clause, a non-literal pattern, any other pattern shape (see
/// [`like_literal_prefix`]), an unindexed column, `DISTINCT`, or any
/// `WHERE` clause that isn't a single top-level `LIKE`/`GLOB` — falling
/// back to the ordinary scan and the existing `like()`/`glob()`
/// function-call filter (`src/codegen/expr/cond.rs`,
/// `src/codegen/expr/value.rs`, untouched).
#[allow(clippy::too_many_arguments)]
pub(super) fn try_compile_like_prefix_seek<F>(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    select: &Select,
    schema: &Rc<TableSchema>,
    cursors: ScanCursors,
    end_label: Label,
    catalog: &[TableSchema],
    sink: &mut F,
) -> Result<bool, CodegenError>
where
    F: FnMut(&mut Emitter, &mut RegAlloc, i32, i32) -> Result<(), CodegenError>,
{
    if matches!(select.distinct, Some(Distinctness::Distinct)) {
        return Ok(false);
    }
    let Some(where_expr) = &select.where_clause else {
        return Ok(false);
    };
    let ExprKind::Like {
        expr,
        pattern,
        glob,
        negated: false,
        escape: None,
    } = &where_expr.kind
    else {
        return Ok(false);
    };
    let Some(col_name) = where_col(expr) else {
        return Ok(false);
    };
    let ExprKind::Literal(Literal::Str(pattern_str)) = &pattern.kind else {
        return Ok(false);
    };
    let Some(prefix) = like_literal_prefix(pattern_str, *glob) else {
        return Ok(false);
    };
    let Some(index_position) = find_leading_index(schema, col_name) else {
        return Ok(false);
    };
    let Some(index) = schema.indexes.get(index_position) else {
        return Ok(false);
    };
    // A prefix seek only compares correctly against what's actually
    // stored in the index if the column keeps text as text — a
    // numeric-affinity column's index entries are the coerced numbers,
    // not the original text, so a text probe would sort into the wrong
    // place (see `is_supported_operand`'s doc for the general hazard).
    if column_affinity(schema, col_name) != Affinity::Text {
        return Ok(false);
    }
    let leading_collation = index
        .columns
        .first()
        .map_or(Collation::Binary, |c| c.collation);

    let index_cursor = cursors.sort;
    open_index_cursor(em, index, index_cursor)?;

    let scope = Scope::single_shared(schema, cursors.table).with_catalog(catalog);
    let limit = compile_limit_setup(em, reg, &scope, select)?;
    let lo_reg = compile_value(em, reg, &scope, &literal_expr(Literal::Str(prefix.clone())))?;
    let hi_reg = compile_value(
        em,
        reg,
        &scope,
        &literal_expr(Literal::Str(prefix_upper_bound(&prefix))),
    )?;

    let seek_addr = em.emit(Instruction::with_p4(
        Opcode::SeekIndexGE,
        index_cursor,
        0,
        lo_reg,
        P4::SeekKey(vec![leading_collation]),
    ));
    em.patch_p2(seek_addr, end_label);

    let loop_start = em.new_label();
    em.place(loop_start);

    let stop_addr = em.emit(Instruction::with_p4(
        Opcode::IdxCompareGT,
        index_cursor,
        0,
        hi_reg,
        P4::SeekKey(vec![leading_collation]),
    ));
    em.patch_p2(stop_addr, end_label);

    let row_skip = em.new_label();
    emit_matched_row(
        em,
        reg,
        select,
        schema,
        cursors,
        index_cursor,
        col_name,
        &limit,
        row_skip,
        end_label,
        catalog,
        sink,
    )?;

    em.place(row_skip);
    let next_addr = em.emit(Instruction::new(Opcode::IdxNext, index_cursor, 0, 0));
    em.patch_p2(next_addr, loop_start);
    Ok(true)
}

/// Compiles `WHERE col IN (v1, v2, ..., vN)` (`col` a plain column with a
/// matching index, every `vI` a literal or bind parameter) as a sequence
/// of `SeekIndexEq` point lookups — one per distinct value — each
/// chaining `IdxRowid` + `SeekRowid` to fetch the full row on a hit and
/// simply skipping to the next value on a miss (unlike the single
/// `end_label`-jumping shape of a point-lookup fast path: a miss on
/// value `vI` must still try `v(I+1)`, only reaching `end_label` once
/// every value has been tried). Duplicate literal values are compiled
/// once, so no row is ever emitted twice. Returns `Ok(false)` for `NOT
/// IN`, an empty list, any non-literal/non-param member, a non-column
/// `expr`, an unindexed column, `DISTINCT`, or any `WHERE` clause that
/// isn't a single top-level `IN (...)` — falling back to the ordinary
/// scan and the existing per-value `Eq`-loop filter
/// (`src/codegen/expr/cond.rs`, untouched).
#[allow(clippy::too_many_arguments)]
pub(super) fn try_compile_in_list_seek<F>(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    select: &Select,
    schema: &Rc<TableSchema>,
    cursors: ScanCursors,
    end_label: Label,
    catalog: &[TableSchema],
    sink: &mut F,
) -> Result<bool, CodegenError>
where
    F: FnMut(&mut Emitter, &mut RegAlloc, i32, i32) -> Result<(), CodegenError>,
{
    if matches!(select.distinct, Some(Distinctness::Distinct)) {
        return Ok(false);
    }
    let Some(where_expr) = &select.where_clause else {
        return Ok(false);
    };
    let ExprKind::In {
        expr,
        list,
        negated: false,
    } = &where_expr.kind
    else {
        return Ok(false);
    };
    if list.is_empty() {
        return Ok(false);
    }
    let Some(col_name) = where_col(expr) else {
        return Ok(false);
    };
    if !list.iter().all(is_supported_operand) {
        return Ok(false);
    }
    let Some(index_position) = find_leading_index(schema, col_name) else {
        return Ok(false);
    };
    let Some(index) = schema.indexes.get(index_position) else {
        return Ok(false);
    };
    let affinity = column_affinity(schema, col_name);
    if !list
        .iter()
        .all(|v| operand_matches_column_affinity(v, affinity))
    {
        return Ok(false);
    }
    let leading_collation = index
        .columns
        .first()
        .map_or(Collation::Binary, |c| c.collation);

    // Dedup identical literal values at compile time (#606) so no row is
    // ever emitted twice; bind parameters are never considered equal to
    // each other or to a literal here (their runtime value is unknown at
    // compile time), so only exact `Literal` duplicates are collapsed.
    let mut seen_literals: Vec<&Literal> = Vec::new();
    let mut operands: Vec<&Expr> = Vec::new();
    for value in list {
        if let ExprKind::Literal(lit) = &value.kind {
            if seen_literals.contains(&lit) {
                continue;
            }
            seen_literals.push(lit);
        }
        operands.push(value);
    }

    let index_cursor = cursors.sort;
    open_index_cursor(em, index, index_cursor)?;

    let scope = Scope::single_shared(schema, cursors.table).with_catalog(catalog);
    let limit = compile_limit_setup(em, reg, &scope, select)?;

    for operand in operands {
        let value_reg = compile_value(em, reg, &scope, operand)?;
        let next_value = em.new_label();
        // #298: a bounded walk with `lo = hi = value`, not a single
        // `SeekIndexEq` -- a non-unique index can hold several entries
        // equal to `value`, and every one of them is a match.
        emit_bounded_index_walk(
            em,
            index_cursor,
            value_reg,
            value_reg,
            leading_collation,
            next_value,
            |em, row_skip| {
                emit_matched_row(
                    em,
                    reg,
                    select,
                    schema,
                    cursors,
                    index_cursor,
                    col_name,
                    &limit,
                    row_skip,
                    end_label,
                    catalog,
                    sink,
                )
            },
        )?;
        em.place(next_value);
    }
    let done_addr = em.emit(Instruction::new(Opcode::Goto, 0, 0, 0));
    em.patch_p2(done_addr, end_label);
    Ok(true)
}

/// Normalizes a single top-level comparison `WHERE` clause to
/// `(col_name, literal_operand, inclusive)` when it has the shape
/// `col > lit`/`col >= lit`/`lit < col`/`lit <= col` — the four spellings
/// of "the index should seek to the first entry strictly/inclusively
/// past `lit` and then walk forward with no upper bound". `col <
/// lit`/`col <= lit`/`lit > col`/`lit >= col` (the descending-bound
/// shapes) return `None`: walking those forward from a low-bound seek
/// would require a *backward* walk from the top of the index, which
/// needs an `IdxLast`/`IdxPrev` stop-check opcode this codegen doesn't
/// have yet (#654) — those shapes keep falling back to the ordinary
/// scan, unchanged from before this function existed.
fn as_forward_comparison(expr: &Expr) -> Option<(&str, &Expr, bool)> {
    let ExprKind::Binary { op, lhs, rhs } = &expr.kind else {
        return None;
    };
    match op {
        BinaryOp::Gt => Some((where_col(lhs)?, rhs.as_ref(), false)),
        BinaryOp::Ge => Some((where_col(lhs)?, rhs.as_ref(), true)),
        BinaryOp::Lt => Some((where_col(rhs)?, lhs.as_ref(), false)),
        BinaryOp::Le => Some((where_col(rhs)?, lhs.as_ref(), true)),
        _ => None,
    }
}

/// Compiles `WHERE col > lit`/`col >= lit`/`lit < col`/`lit <= col` (`col`
/// a plain column with a matching index) as a `SeekIndexGE(lit)` walk
/// with no upper bound — `col >= lit`/`lit <= col` (`inclusive`) process
/// every entry the seek lands on and after; the exclusive `>`/`<` shapes
/// additionally skip a leading run of entries equal to `lit` (duplicate
/// keys) before processing, since `SeekIndexGE`'s floor is inclusive.
/// Real sqlite3's own `EXPLAIN QUERY PLAN` collapses inclusive and
/// exclusive into the same `(col>?)` wording (confirmed empirically,
/// sqlite3 3.53.4) — [`find_range_seek_detail`] mirrors that, not a
/// `>=`-specific spelling. Returns `Ok(false)` — `em`/`reg` untouched —
/// for any shape [`as_forward_comparison`] doesn't recognize, an
/// unsupported/mismatched-affinity operand, an unindexed column, or
/// `DISTINCT`.
#[allow(clippy::too_many_arguments)]
pub(super) fn try_compile_forward_comparison_seek<F>(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    select: &Select,
    schema: &Rc<TableSchema>,
    cursors: ScanCursors,
    end_label: Label,
    catalog: &[TableSchema],
    sink: &mut F,
) -> Result<bool, CodegenError>
where
    F: FnMut(&mut Emitter, &mut RegAlloc, i32, i32) -> Result<(), CodegenError>,
{
    if matches!(select.distinct, Some(Distinctness::Distinct)) {
        return Ok(false);
    }
    let Some(where_expr) = &select.where_clause else {
        return Ok(false);
    };
    let Some((col_name, operand, inclusive)) = as_forward_comparison(where_expr) else {
        return Ok(false);
    };
    let scope = Scope::single_shared(schema, cursors.table).with_catalog(catalog);
    if !is_constant_operand(operand, &scope) {
        return Ok(false);
    }
    let Some(index_position) = find_leading_index(schema, col_name) else {
        return Ok(false);
    };
    let Some(index) = schema.indexes.get(index_position) else {
        return Ok(false);
    };
    let affinity = column_affinity(schema, col_name);
    if !operand_matches_column_affinity(operand, affinity) {
        return Ok(false);
    }
    let leading_collation = index
        .columns
        .first()
        .map_or(Collation::Binary, |c| c.collation);

    let index_cursor = cursors.sort;
    open_index_cursor(em, index, index_cursor)?;

    let limit = compile_limit_setup(em, reg, &scope, select)?;
    let bound_reg = compile_value(em, reg, &scope, operand)?;

    // #280: see `try_compile_between_seek`'s identical comment.
    let bound_null_addr = em.emit(Instruction::new(Opcode::IsNull, bound_reg, 0, 0));
    em.patch_p2(bound_null_addr, end_label);

    let seek_addr = em.emit(Instruction::with_p4(
        Opcode::SeekIndexGE,
        index_cursor,
        0,
        bound_reg,
        P4::SeekKey(vec![leading_collation]),
    ));
    em.patch_p2(seek_addr, end_label);

    if !inclusive {
        // The seek floor is inclusive, so a run of entries equal to
        // `bound_reg` (duplicate keys) needs skipping before the walk
        // below can treat "landed here" as "strictly past the bound".
        let skip_start = em.new_label();
        em.place(skip_start);
        let past_bound = em.new_label();
        let gt_addr = em.emit(Instruction::with_p4(
            Opcode::IdxCompareGT,
            index_cursor,
            0,
            bound_reg,
            P4::SeekKey(vec![leading_collation]),
        ));
        em.patch_p2(gt_addr, past_bound);
        let skip_next_addr = em.emit(Instruction::new(Opcode::IdxNext, index_cursor, 0, 0));
        em.patch_p2(skip_next_addr, skip_start);
        let exhausted_addr = em.emit(Instruction::new(Opcode::Goto, 0, 0, 0));
        em.patch_p2(exhausted_addr, end_label);
        em.place(past_bound);
    }

    let loop_start = em.new_label();
    em.place(loop_start);
    let row_skip = em.new_label();
    emit_matched_row(
        em,
        reg,
        select,
        schema,
        cursors,
        index_cursor,
        col_name,
        &limit,
        row_skip,
        end_label,
        catalog,
        sink,
    )?;
    em.place(row_skip);
    let next_addr = em.emit(Instruction::new(Opcode::IdxNext, index_cursor, 0, 0));
    em.patch_p2(next_addr, loop_start);
    Ok(true)
}

/// The `schema.indexes` position of the index a range-seek fast path in
/// this file would pick for `where_expr`, or `None` if `where_expr`
/// doesn't match any of the recognized shapes (`BETWEEN`, `LIKE`/`GLOB`
/// prefix, `IN`, or a forward comparison) — the same
/// shape-recognition/affinity checks `try_compile_range_row_seek` itself
/// applies, factored out so a caller can inspect *which* index would be
/// scanned without actually emitting anything (`update.rs`'s #675 fix
/// uses this to decide whether the `SET` clause touches that index and a
/// two-pass ephemeral-rowid plan is still required).
pub(crate) fn range_seek_index_position(
    where_expr: &Expr,
    schema: &TableSchema,
    catalog: &[TableSchema],
) -> Option<usize> {
    // #280: only the cursor id matters for `is_constant_operand`'s own
    // purposes not at all -- it never emits anything, only resolves a
    // `Subquery` operand's own `FROM` against `catalog` -- so `0` here
    // is a placeholder, never an actual cursor this function opens.
    let scope = Scope::single(schema, 0).with_catalog(catalog);
    if let Some((col, lo, hi, _)) = as_bounds(where_expr) {
        let col_name = where_col(col)?;
        if !is_constant_operand(lo, &scope) || !is_constant_operand(hi, &scope) {
            return None;
        }
        let index_position = find_leading_index(schema, col_name)?;
        let affinity = column_affinity(schema, col_name);
        if !operand_matches_column_affinity(lo, affinity)
            || !operand_matches_column_affinity(hi, affinity)
        {
            return None;
        }
        return Some(index_position);
    }
    match &where_expr.kind {
        ExprKind::Like {
            expr,
            pattern,
            glob,
            negated: false,
            escape: None,
        } => {
            let col_name = where_col(expr)?;
            let ExprKind::Literal(Literal::Str(pattern_str)) = &pattern.kind else {
                return None;
            };
            like_literal_prefix(pattern_str, *glob)?;
            let index_position = find_leading_index(schema, col_name)?;
            if column_affinity(schema, col_name) != Affinity::Text {
                return None;
            }
            Some(index_position)
        }
        ExprKind::In {
            expr,
            list,
            negated: false,
        } => {
            if list.is_empty() || !list.iter().all(is_supported_operand) {
                return None;
            }
            let col_name = where_col(expr)?;
            let index_position = find_leading_index(schema, col_name)?;
            let affinity = column_affinity(schema, col_name);
            if !list
                .iter()
                .all(|v| operand_matches_column_affinity(v, affinity))
            {
                return None;
            }
            Some(index_position)
        }
        _ => as_forward_comparison(where_expr).and_then(|(col_name, operand, _inclusive)| {
            if !is_constant_operand(operand, &scope) {
                return None;
            }
            let index_position = find_leading_index(schema, col_name)?;
            let affinity = column_affinity(schema, col_name);
            if !operand_matches_column_affinity(operand, affinity) {
                return None;
            }
            Some(index_position)
        }),
    }
}

/// `EXPLAIN QUERY PLAN` reporting for this file's fast paths (#606's
/// acceptance criteria: `EXPLAIN QUERY PLAN` must show index usage for
/// these query shapes) — reuses the exact same shape-recognition
/// helpers (`where_col`/`is_supported_operand`/`find_leading_index`/
/// `like_literal_prefix`) the actual codegen functions above use, so
/// this report can never drift from what `compile_direct_scan` really
/// takes. `table_display` is the already-resolved display name for the
/// table (`eqp_display_name` in `eqp.rs`) — this function only ever
/// needs it for formatting.
pub(super) fn find_range_seek_detail(
    schema: &TableSchema,
    select: &Select,
    table_display: &str,
    catalog: &[TableSchema],
) -> Option<String> {
    // #280: see `range_seek_index_position`'s identical comment -- `0`
    // is a placeholder cursor id, never opened.
    let scope = Scope::single(schema, 0).with_catalog(catalog);
    let where_expr = select.where_clause.as_ref()?;
    if let Some((col, lo, hi, equality)) = as_bounds(where_expr) {
        let col_name = where_col(col)?;
        if !is_constant_operand(lo, &scope) || !is_constant_operand(hi, &scope) {
            return None;
        }
        let index_position = find_leading_index(schema, col_name)?;
        let index = schema.indexes.get(index_position)?;
        let affinity = column_affinity(schema, col_name);
        if !operand_matches_column_affinity(lo, affinity)
            || !operand_matches_column_affinity(hi, affinity)
        {
            return None;
        }
        // sqlite3's wording: `(col=?)` for equality, `(col>? AND col<?)`
        // for BETWEEN (inclusive bounds notwithstanding -- confirmed
        // empirically, sqlite3 3.53.4).
        return Some(if equality {
            format!(
                "SEARCH {table_display} USING INDEX {} ({col_name}=?)",
                index.name
            )
        } else {
            format!(
                "SEARCH {table_display} USING INDEX {} ({col_name}>? AND {col_name}<?)",
                index.name
            )
        });
    }
    match &where_expr.kind {
        ExprKind::Like {
            expr,
            pattern,
            glob,
            negated: false,
            escape: None,
        } => {
            let col_name = where_col(expr)?;
            let ExprKind::Literal(Literal::Str(pattern_str)) = &pattern.kind else {
                return None;
            };
            like_literal_prefix(pattern_str, *glob)?;
            let index_position = find_leading_index(schema, col_name)?;
            let index = schema.indexes.get(index_position)?;
            if column_affinity(schema, col_name) != Affinity::Text {
                return None;
            }
            Some(format!(
                "SEARCH {table_display} USING INDEX {} ({col_name}>? AND {col_name}<?)",
                index.name
            ))
        }
        ExprKind::In {
            expr,
            list,
            negated: false,
        } => {
            if list.is_empty() || !list.iter().all(is_supported_operand) {
                return None;
            }
            let col_name = where_col(expr)?;
            let index_position = find_leading_index(schema, col_name)?;
            let index = schema.indexes.get(index_position)?;
            let affinity = column_affinity(schema, col_name);
            if !list
                .iter()
                .all(|v| operand_matches_column_affinity(v, affinity))
            {
                return None;
            }
            Some(format!(
                "SEARCH {table_display} USING INDEX {} ({col_name}=?)",
                index.name
            ))
        }
        _ => as_forward_comparison(where_expr).and_then(|(col_name, operand, _inclusive)| {
            if !is_constant_operand(operand, &scope) {
                return None;
            }
            let index_position = find_leading_index(schema, col_name)?;
            let index = schema.indexes.get(index_position)?;
            let affinity = column_affinity(schema, col_name);
            if !operand_matches_column_affinity(operand, affinity) {
                return None;
            }
            // Real sqlite3 collapses inclusive and exclusive into the
            // same `(col>?)` wording (see `try_compile_forward_comparison_seek`'s
            // doc) -- `_inclusive` only matters to the compiled seek's
            // dup-skip, not to this report.
            Some(format!(
                "SEARCH {table_display} USING INDEX {} ({col_name}>?)",
                index.name
            ))
        }),
    }
}

/// A `WHERE` clause that bounds one indexed column from both sides:
/// `col BETWEEN lo AND hi`, or (#298) an equality `col = lit` / `lit =
/// col`, which is the same walk with `lo` and `hi` the *same* expression.
/// Returns `(col, lo, hi, equality)` -- `equality` records which spelling
/// it was, for `EXPLAIN QUERY PLAN`'s wording. `None` for every other
/// shape. (A tuple rather than a struct: the qualified subset,
/// `make check-mvl-limit`, admits no explicit lifetime parameters.)
fn as_bounds(where_expr: &Expr) -> Option<(&Expr, &Expr, &Expr, bool)> {
    match &where_expr.kind {
        ExprKind::Between {
            expr,
            lo,
            hi,
            negated: false,
        } => Some((expr, lo, hi, false)),
        ExprKind::Binary {
            op: BinaryOp::Eq,
            lhs,
            rhs,
        } => {
            let (col, lit) = if where_col(lhs).is_some() {
                (lhs.as_ref(), rhs.as_ref())
            } else if where_col(rhs).is_some() {
                (rhs.as_ref(), lhs.as_ref())
            } else {
                return None;
            };
            Some((col, lit, lit, true))
        }
        _ => None,
    }
}

/// The one correct "every index entry with `lo <= key <= hi`" walk, shared
/// by `BETWEEN`, `=` and each `IN (...)` value (#298): a `NULL` bound
/// matches nothing (jump to `exit`); `SeekIndexGE(lo)` positions on the
/// first candidate (none: `exit`); then loop -- `IdxCompareGT(hi)` exits
/// once past the upper bound, `body` handles the positioned entry (it
/// receives `row_skip` to jump to on a missing table row), `IdxNext`
/// advances and re-enters the loop. A single `SeekIndexEq` is *not* a
/// substitute: it lands on one entry, and a non-unique index may hold many
/// equal to `lo`.
fn emit_bounded_index_walk<B>(
    em: &mut Emitter,
    index_cursor: i32,
    lo_reg: i32,
    hi_reg: i32,
    leading_collation: Collation,
    exit: Label,
    body: B,
) -> Result<(), CodegenError>
where
    B: FnOnce(&mut Emitter, Label) -> Result<(), CodegenError>,
{
    let lo_null_addr = em.emit(Instruction::new(Opcode::IsNull, lo_reg, 0, 0));
    em.patch_p2(lo_null_addr, exit);
    if hi_reg != lo_reg {
        let hi_null_addr = em.emit(Instruction::new(Opcode::IsNull, hi_reg, 0, 0));
        em.patch_p2(hi_null_addr, exit);
    }
    let seek_addr = em.emit(Instruction::with_p4(
        Opcode::SeekIndexGE,
        index_cursor,
        0,
        lo_reg,
        P4::SeekKey(vec![leading_collation]),
    ));
    em.patch_p2(seek_addr, exit);
    let loop_start = em.new_label();
    em.place(loop_start);
    let stop_addr = em.emit(Instruction::with_p4(
        Opcode::IdxCompareGT,
        index_cursor,
        0,
        hi_reg,
        P4::SeekKey(vec![leading_collation]),
    ));
    em.patch_p2(stop_addr, exit);
    let row_skip = em.new_label();
    body(em, row_skip)?;
    em.place(row_skip);
    let next_addr = em.emit(Instruction::new(Opcode::IdxNext, index_cursor, 0, 0));
    em.patch_p2(next_addr, loop_start);
    Ok(())
}

#[cfg(test)]
#[allow(non_snake_case)]
mod mcdc_vectors {
    //! Tagged MC/DC vectors for this file's multi-leaf decisions
    //! (`mcdc__<id>__vN`, joined to `tests/mcdc/obligations.json`
    //! by `make test-mcdc`; db-core#219/#235).

    use crate::codegen::row::explain_query_plan;
    use crate::codegen::row::select::range_seek_index_position;
    use crate::codegen::row::{
        compile_select_with_catalog, compile_update, IndexSchema, IndexedColumn, TableSchema,
    };
    use crate::parser::ast::{Expr, ExprKind, Select, Update};
    use crate::parser::row::error::{parse_select, parse_update, ParseOutcome};
    use crate::parser::Span;
    use crate::value::Collation;
    use crate::vm::row::{Opcode, Program};
    use std::collections::HashMap;

    const INDEX_ROOT: i32 = 3;

    fn select(sql: &str) -> Select {
        match parse_select(sql) {
            ParseOutcome::Accepted(s) => *s,
            other => panic!("expected {sql:?} to parse as a SELECT, got {other:?}"),
        }
    }

    fn update(sql: &str) -> Update {
        match parse_update(sql) {
            ParseOutcome::Accepted(u) => *u,
            other => panic!("expected {sql:?} to parse as an UPDATE, got {other:?}"),
        }
    }

    fn where_of(sql: &str) -> Expr {
        select(sql).where_clause.expect("WHERE clause")
    }

    fn column_expr(name: &str) -> Expr {
        Expr {
            kind: ExprKind::Column {
                table: None,
                catalog: None,
                name: name.to_string(),
            },
            span: Span {
                line: 0,
                column: 0,
                offset: 0,
                len: 0,
            },
        }
    }

    /// Table `t(a <a_type>, b TEXT)` rooted at page 2, with one index `idx`
    /// on `a` (root page 3) when `indexed`, and `a` as the rowid alias when
    /// `alias`.
    fn schema(a_type: &str, indexed: bool, alias: bool) -> TableSchema {
        let indexes = if indexed {
            vec![IndexSchema {
                name: "idx".to_string(),
                unique: false,
                columns: vec![IndexedColumn {
                    name: "a".to_string(),
                    desc: false,
                    collation: Collation::Binary,
                }],
                root_page: 3,
            }]
        } else {
            vec![]
        };
        TableSchema {
            name: "t".to_string(),
            root_page: 2,
            columns: vec!["a".to_string(), "b".to_string()],
            column_types: vec![a_type.to_string(), "TEXT".to_string()],
            column_collations: vec![Collation::Binary, Collation::Binary],
            sql: format!("CREATE TABLE t (a {a_type}, b TEXT)"),
            indexes,
            rowid_alias: if alias { Some(0) } else { None },
            ..Default::default()
        }
    }

    fn compile(sql: &str, schema: &TableSchema) -> Program {
        compile_select_with_catalog(&select(sql), schema, std::slice::from_ref(schema))
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
    }

    fn compile_upd(sql: &str, schema: &TableSchema) -> Program {
        compile_update(&update(sql), schema).unwrap_or_else(|e| panic!("{sql}: {e:?}"))
    }

    fn has(program: &Program, opcode: Opcode) -> bool {
        program.instructions.iter().any(|i| i.opcode == opcode)
    }

    fn seeks(program: &Program) -> bool {
        has(program, Opcode::SeekIndexGE)
    }

    /// The cursor slot `OpenRead` bound to the index root page, if any.
    fn index_cursor(program: &Program) -> Option<i32> {
        program
            .instructions
            .iter()
            .find(|i| i.opcode == Opcode::OpenRead && i.p2 == INDEX_ROOT)
            .map(|i| i.p1)
    }

    fn reads_column_from_index(program: &Program) -> bool {
        let Some(cursor) = index_cursor(program) else {
            return false;
        };
        program
            .instructions
            .iter()
            .any(|i| i.opcode == Opcode::Column && i.p1 == cursor)
    }

    fn eqp_detail(sql: &str, schema: &TableSchema) -> String {
        let rows = explain_query_plan(
            &select(sql),
            std::slice::from_ref(schema),
            &HashMap::new(),
            std::slice::from_ref(schema),
        )
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
        rows.first().map(|r| r.detail.clone()).unwrap_or_default()
    }

    // --- range_scan_327: `name == indexed_col && rowid_alias != idx` -------
    #[test]
    fn mcdc__codegen_row_select_range_scan_emit_matched_row_79427c66__v1_indexed_non_alias_column_is_read_from_the_index_cursor(
    ) {
        let p = compile(
            "SELECT a, b FROM t WHERE a BETWEEN 1 AND 5",
            &schema("INTEGER", true, false),
        );
        assert!(seeks(&p));
        assert!(reads_column_from_index(&p));
    }

    #[test]
    fn mcdc__codegen_row_select_range_scan_emit_matched_row_79427c66__v2_indexed_rowid_alias_column_is_read_via_rowid(
    ) {
        let p = compile(
            "SELECT a, b FROM t WHERE a BETWEEN 1 AND 5",
            &schema("INTEGER", true, true),
        );
        assert!(seeks(&p));
        assert!(has(&p, Opcode::Rowid));
        assert!(!reads_column_from_index(&p));
    }

    #[test]
    fn mcdc__codegen_row_select_range_scan_emit_matched_row_79427c66__v3_non_indexed_column_is_read_from_the_table_cursor(
    ) {
        let p = compile(
            "SELECT b FROM t WHERE a BETWEEN 1 AND 5",
            &schema("INTEGER", true, false),
        );
        assert!(seeks(&p));
        assert!(!reads_column_from_index(&p));
    }

    // --- range_scan_382: BETWEEN operands supported -----------------------
    #[test]
    fn mcdc__codegen_row_select_range_scan_try_compile_between_seek_3f07d1cf__v1_both_bounds_literal_seeks(
    ) {
        let p = compile(
            "SELECT b FROM t WHERE a BETWEEN 1 AND 5",
            &schema("INTEGER", true, false),
        );
        assert!(seeks(&p));
    }

    #[test]
    fn mcdc__codegen_row_select_range_scan_try_compile_between_seek_3f07d1cf__v2_computed_lower_bound_seeks(
    ) {
        // #280: a constant-arithmetic bound is loop-constant, computed
        // once before the seek same as a bare literal.
        let p = compile(
            "SELECT b FROM t WHERE a BETWEEN 1 + 1 AND 5",
            &schema("INTEGER", true, false),
        );
        assert!(seeks(&p));
    }

    #[test]
    fn mcdc__codegen_row_select_range_scan_try_compile_between_seek_3f07d1cf__v3_computed_upper_bound_seeks(
    ) {
        let p = compile(
            "SELECT b FROM t WHERE a BETWEEN 1 AND 5 + 1",
            &schema("INTEGER", true, false),
        );
        assert!(seeks(&p));
    }

    // --- range_scan_392: BETWEEN operands match column affinity ----------
    #[test]
    fn mcdc__codegen_row_select_range_scan_try_compile_between_seek_3962915e__v1_both_bounds_match_integer_affinity_seeks(
    ) {
        let p = compile(
            "SELECT b FROM t WHERE a BETWEEN 1 AND 5",
            &schema("INTEGER", true, false),
        );
        assert!(seeks(&p));
    }

    #[test]
    fn mcdc__codegen_row_select_range_scan_try_compile_between_seek_3962915e__v2_text_lower_bound_against_integer_column_scans(
    ) {
        let p = compile(
            "SELECT b FROM t WHERE a BETWEEN 'x' AND 5",
            &schema("INTEGER", true, false),
        );
        assert!(!seeks(&p));
    }

    #[test]
    fn mcdc__codegen_row_select_range_scan_try_compile_between_seek_3962915e__v3_text_upper_bound_against_integer_column_scans(
    ) {
        let p = compile(
            "SELECT b FROM t WHERE a BETWEEN 1 AND 'x'",
            &schema("INTEGER", true, false),
        );
        assert!(!seeks(&p));
    }

    // ---------------------------------------------------------------------
    // #280: an uncorrelated scalar subquery (or constant arithmetic
    // composed of one) as a range-seek bound is now loop-constant-eligible
    // -- computed once before the seek, same as a literal. A correlated
    // subquery bound, or a bare `NULL` bound, still isn't eligible/seeks
    // an empty range respectively.
    // ---------------------------------------------------------------------
    #[test]
    fn range_seek_280_uncorrelated_subquery_bound_seeks() {
        let p = compile(
            "SELECT b FROM t WHERE a > (SELECT avg(a) FROM t)",
            &schema("INTEGER", true, false),
        );
        assert!(seeks(&p));
    }

    #[test]
    fn range_seek_280_correlated_subquery_bound_falls_back_to_scan() {
        let s = TableSchema {
            name: "s".to_string(),
            root_page: 9,
            columns: vec!["y".to_string()],
            column_types: vec!["INTEGER".to_string()],
            column_collations: vec![Collation::Binary],
            sql: "CREATE TABLE s (y INTEGER)".to_string(),
            ..Default::default()
        };
        let t = schema("INTEGER", true, false);
        let p = compile_select_with_catalog(
            &select("SELECT b FROM t WHERE a > (SELECT avg(y) FROM s WHERE s.y = t.a)"),
            &t,
            &[t.clone(), s],
        )
        .expect("expected the query to compile");
        assert!(!seeks(&p));
        assert!(has(&p, Opcode::Rewind));
    }

    #[test]
    fn range_seek_280_null_bound_seeks_an_empty_range() {
        let p = compile(
            "SELECT b FROM t WHERE a > (SELECT avg(a) FROM t WHERE 0)",
            &schema("INTEGER", true, false),
        );
        assert!(seeks(&p));
        assert!(has(&p, Opcode::IsNull));
    }

    // --- range_scan_518: row-seek (UPDATE) BETWEEN operands supported -----
    #[test]
    fn mcdc__codegen_row_select_range_scan_try_compile_range_row_seek_ab031ffa__v1_update_between_literals_seeks(
    ) {
        let p = compile_upd(
            "UPDATE t SET b = 'z' WHERE a BETWEEN 1 AND 5",
            &schema("INTEGER", true, false),
        );
        assert!(seeks(&p));
    }

    #[test]
    fn mcdc__codegen_row_select_range_scan_try_compile_range_row_seek_ab031ffa__v2_update_computed_lower_bound_seeks(
    ) {
        // #280: see `mcdc__codegen_row_select_range_scan_try_compile_between_seek_3f07d1cf__v2`'s identical comment.
        let p = compile_upd(
            "UPDATE t SET b = 'z' WHERE a BETWEEN 1 + 1 AND 5",
            &schema("INTEGER", true, false),
        );
        assert!(seeks(&p));
    }

    #[test]
    fn mcdc__codegen_row_select_range_scan_try_compile_range_row_seek_ab031ffa__v3_update_computed_upper_bound_seeks(
    ) {
        let p = compile_upd(
            "UPDATE t SET b = 'z' WHERE a BETWEEN 1 AND 5 + 1",
            &schema("INTEGER", true, false),
        );
        assert!(seeks(&p));
    }

    // --- range_scan_528: row-seek (UPDATE) BETWEEN affinity ---------------
    #[test]
    fn mcdc__codegen_row_select_range_scan_try_compile_range_row_seek_3962915e__v1_update_bounds_match_affinity_seeks(
    ) {
        let p = compile_upd(
            "UPDATE t SET b = 'z' WHERE a BETWEEN 1 AND 5",
            &schema("INTEGER", true, false),
        );
        assert!(seeks(&p));
    }

    #[test]
    fn mcdc__codegen_row_select_range_scan_try_compile_range_row_seek_3962915e__v2_update_text_lower_bound_scans(
    ) {
        let p = compile_upd(
            "UPDATE t SET b = 'z' WHERE a BETWEEN 'x' AND 5",
            &schema("INTEGER", true, false),
        );
        assert!(!seeks(&p));
    }

    #[test]
    fn mcdc__codegen_row_select_range_scan_try_compile_range_row_seek_3962915e__v3_update_text_upper_bound_scans(
    ) {
        let p = compile_upd(
            "UPDATE t SET b = 'z' WHERE a BETWEEN 1 AND 'x'",
            &schema("INTEGER", true, false),
        );
        assert!(!seeks(&p));
    }

    // --- codegen_row_select_range_scan_like_literal_prefix_e3457195: LIKE prefix contains wildcard / single-char ------
    #[test]
    fn mcdc__codegen_row_select_range_scan_like_literal_prefix_e3457195__v1_plain_prefix_seeks() {
        let p = compile(
            "SELECT b FROM t WHERE a LIKE 'ab%'",
            &schema("TEXT", true, false),
        );
        assert!(seeks(&p));
    }

    #[test]
    fn mcdc__codegen_row_select_range_scan_like_literal_prefix_e3457195__v2_prefix_with_inner_percent_scans(
    ) {
        let p = compile(
            "SELECT b FROM t WHERE a LIKE 'a%b%'",
            &schema("TEXT", true, false),
        );
        assert!(!seeks(&p));
    }

    #[test]
    fn mcdc__codegen_row_select_range_scan_like_literal_prefix_e3457195__v3_prefix_with_underscore_scans(
    ) {
        let p = compile(
            "SELECT b FROM t WHERE a LIKE 'a_b%'",
            &schema("TEXT", true, false),
        );
        assert!(!seeks(&p));
    }

    // --- range_scan_672: `!glob && prefix has backslash` ------------------
    #[test]
    fn mcdc__codegen_row_select_range_scan_like_literal_prefix_492d32fb__v1_like_with_backslash_in_prefix_scans(
    ) {
        let p = compile(
            "SELECT b FROM t WHERE a LIKE 'a\\b%'",
            &schema("TEXT", true, false),
        );
        assert!(!seeks(&p));
    }

    #[test]
    fn mcdc__codegen_row_select_range_scan_like_literal_prefix_492d32fb__v2_glob_with_backslash_in_prefix_seeks(
    ) {
        let p = compile(
            "SELECT b FROM t WHERE a GLOB 'a\\b*'",
            &schema("TEXT", true, false),
        );
        assert!(seeks(&p));
    }

    #[test]
    fn mcdc__codegen_row_select_range_scan_like_literal_prefix_492d32fb__v3_like_without_backslash_seeks(
    ) {
        let p = compile(
            "SELECT b FROM t WHERE a LIKE 'ab%'",
            &schema("TEXT", true, false),
        );
        assert!(seeks(&p));
    }

    // --- range_scan_1109: range_seek_index_position BETWEEN operands ------
    #[test]
    fn mcdc__codegen_row_select_range_scan_range_seek_index_position_3f07d1cf__v1_literal_bounds_pick_the_index(
    ) {
        let s = schema("INTEGER", true, false);
        let w = where_of("SELECT b FROM t WHERE a BETWEEN 1 AND 5");
        assert_eq!(
            range_seek_index_position(&w, &s, std::slice::from_ref(&s)),
            Some(0)
        );
    }

    #[test]
    fn mcdc__codegen_row_select_range_scan_range_seek_index_position_3f07d1cf__v2_computed_lower_bound_picks_the_index(
    ) {
        // #280: a constant-arithmetic bound is loop-constant.
        let s = schema("INTEGER", true, false);
        let w = where_of("SELECT b FROM t WHERE a BETWEEN 1 + 1 AND 5");
        assert_eq!(
            range_seek_index_position(&w, &s, std::slice::from_ref(&s)),
            Some(0)
        );
    }

    #[test]
    fn mcdc__codegen_row_select_range_scan_range_seek_index_position_3f07d1cf__v3_computed_upper_bound_picks_the_index(
    ) {
        let s = schema("INTEGER", true, false);
        let w = where_of("SELECT b FROM t WHERE a BETWEEN 1 AND 5 + 1");
        assert_eq!(
            range_seek_index_position(&w, &s, std::slice::from_ref(&s)),
            Some(0)
        );
    }

    // --- range_scan_1114: range_seek_index_position BETWEEN affinity ------
    #[test]
    fn mcdc__codegen_row_select_range_scan_range_seek_index_position_3962915e__v1_matching_affinity_picks_the_index(
    ) {
        let s = schema("INTEGER", true, false);
        let w = where_of("SELECT b FROM t WHERE a BETWEEN 1 AND 5");
        assert_eq!(
            range_seek_index_position(&w, &s, std::slice::from_ref(&s)),
            Some(0)
        );
    }

    #[test]
    fn mcdc__codegen_row_select_range_scan_range_seek_index_position_3962915e__v2_text_lower_bound_picks_no_index(
    ) {
        let s = schema("INTEGER", true, false);
        let w = where_of("SELECT b FROM t WHERE a BETWEEN 'x' AND 5");
        assert_eq!(
            range_seek_index_position(&w, &s, std::slice::from_ref(&s)),
            None
        );
    }

    #[test]
    fn mcdc__codegen_row_select_range_scan_range_seek_index_position_3962915e__v3_text_upper_bound_picks_no_index(
    ) {
        let s = schema("INTEGER", true, false);
        let w = where_of("SELECT b FROM t WHERE a BETWEEN 1 AND 'x'");
        assert_eq!(
            range_seek_index_position(&w, &s, std::slice::from_ref(&s)),
            None
        );
    }

    // --- codegen_row_select_range_scan_find_range_seek_detail_3f07d1cf: range_seek_index_position IN list ---------------
    #[test]
    fn mcdc__codegen_row_select_range_scan_range_seek_index_position_02599d5e__v1_non_empty_literal_list_picks_the_index(
    ) {
        let s = schema("INTEGER", true, false);
        let w = where_of("SELECT b FROM t WHERE a IN (1, 2)");
        assert_eq!(
            range_seek_index_position(&w, &s, std::slice::from_ref(&s)),
            Some(0)
        );
    }

    #[test]
    fn mcdc__codegen_row_select_range_scan_range_seek_index_position_02599d5e__v2_empty_list_picks_no_index(
    ) {
        let s = schema("INTEGER", true, false);
        let w = Expr {
            kind: ExprKind::In {
                expr: Box::new(column_expr("a")),
                list: vec![],
                negated: false,
            },
            span: column_expr("a").span,
        };
        assert_eq!(
            range_seek_index_position(&w, &s, std::slice::from_ref(&s)),
            None
        );
    }

    #[test]
    fn mcdc__codegen_row_select_range_scan_range_seek_index_position_02599d5e__v3_computed_list_member_picks_no_index(
    ) {
        let s = schema("INTEGER", true, false);
        let w = where_of("SELECT b FROM t WHERE a IN (1, 1 + 1)");
        assert_eq!(
            range_seek_index_position(&w, &s, std::slice::from_ref(&s)),
            None
        );
    }

    // --- codegen_row_select_range_scan_find_range_seek_detail_3f07d1cf: EQP BETWEEN/= operands loop-constant ------------------
    #[test]
    fn mcdc__codegen_row_select_range_scan_find_range_seek_detail_3f07d1cf__v1_eqp_reports_index_search_for_literal_bounds(
    ) {
        let d = eqp_detail(
            "SELECT b FROM t WHERE a BETWEEN 1 AND 5",
            &schema("INTEGER", true, false),
        );
        assert!(d.contains("USING INDEX idx"), "{d}");
    }

    #[test]
    fn mcdc__codegen_row_select_range_scan_find_range_seek_detail_3f07d1cf__v2_eqp_reports_index_search_for_computed_lower_bound(
    ) {
        // #280: a constant-arithmetic bound is loop-constant.
        let d = eqp_detail(
            "SELECT b FROM t WHERE a BETWEEN 1 + 1 AND 5",
            &schema("INTEGER", true, false),
        );
        assert!(d.contains("USING INDEX"), "{d}");
    }

    #[test]
    fn mcdc__codegen_row_select_range_scan_find_range_seek_detail_3f07d1cf__v3_eqp_reports_index_search_for_computed_upper_bound(
    ) {
        let d = eqp_detail(
            "SELECT b FROM t WHERE a BETWEEN 1 AND 5 + 1",
            &schema("INTEGER", true, false),
        );
        assert!(d.contains("USING INDEX"), "{d}");
    }

    // --- range_scan_1205: EQP BETWEEN affinity ----------------------------
    #[test]
    fn mcdc__codegen_row_select_range_scan_find_range_seek_detail_3962915e__v1_eqp_reports_index_search_when_affinity_matches(
    ) {
        let d = eqp_detail(
            "SELECT b FROM t WHERE a BETWEEN 1 AND 5",
            &schema("INTEGER", true, false),
        );
        assert!(d.contains("USING INDEX idx"), "{d}");
    }

    #[test]
    fn mcdc__codegen_row_select_range_scan_find_range_seek_detail_3962915e__v2_eqp_reports_scan_for_text_lower_bound(
    ) {
        let d = eqp_detail(
            "SELECT b FROM t WHERE a BETWEEN 'x' AND 5",
            &schema("INTEGER", true, false),
        );
        assert!(!d.contains("USING INDEX"), "{d}");
    }

    #[test]
    fn mcdc__codegen_row_select_range_scan_find_range_seek_detail_3962915e__v3_eqp_reports_scan_for_text_upper_bound(
    ) {
        let d = eqp_detail(
            "SELECT b FROM t WHERE a BETWEEN 1 AND 'x'",
            &schema("INTEGER", true, false),
        );
        assert!(!d.contains("USING INDEX"), "{d}");
    }

    // --- range_scan_1242: EQP IN list -------------------------------------
    #[test]
    fn mcdc__codegen_row_select_range_scan_find_range_seek_detail_02599d5e__v1_eqp_reports_index_search_for_literal_list(
    ) {
        let d = eqp_detail(
            "SELECT b FROM t WHERE a IN (1, 2)",
            &schema("INTEGER", true, false),
        );
        assert!(d.contains("USING INDEX idx"), "{d}");
    }

    #[test]
    fn mcdc__codegen_row_select_range_scan_find_range_seek_detail_02599d5e__v2_eqp_reports_scan_for_empty_list(
    ) {
        let s = schema("INTEGER", true, false);
        let mut sel = select("SELECT b FROM t WHERE a IN (1)");
        sel.where_clause = Some(Expr {
            kind: ExprKind::In {
                expr: Box::new(column_expr("a")),
                list: vec![],
                negated: false,
            },
            span: column_expr("a").span,
        });
        let rows = explain_query_plan(
            &sel,
            std::slice::from_ref(&s),
            &HashMap::new(),
            std::slice::from_ref(&s),
        )
        .unwrap();
        let d = rows.first().map(|r| r.detail.clone()).unwrap_or_default();
        assert!(!d.contains("USING INDEX"), "{d}");
    }

    #[test]
    fn mcdc__codegen_row_select_range_scan_find_range_seek_detail_02599d5e__v3_eqp_reports_scan_for_computed_list_member(
    ) {
        let d = eqp_detail(
            "SELECT b FROM t WHERE a IN (1, 1 + 1)",
            &schema("INTEGER", true, false),
        );
        assert!(!d.contains("USING INDEX"), "{d}");
    }
}
