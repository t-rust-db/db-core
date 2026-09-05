//! Index range seeks (db-core#94, ADR 0034) -- ported from sqlite-rs's
//! `src/codegen/select/range_scan.rs`.
//!
//! A `WHERE` clause bounding an indexed column compiles to a
//! `SeekIndexGE` onto the low bound followed by an `IdxCompareGT`-
//! guarded `IdxNext` walk, plus `IdxRowid` + `SeekRowid` per matching
//! entry, in place of [`super::select`]'s `Rewind`/`Next` scan +
//! [`super::cond`] filter (which is untouched, and still handles every
//! other shape).
//!
//! **Scoped to the single mechanically-decidable case**, mirroring
//! [`super`]'s own note on `planner.rs`: an index whose *leading*
//! column is the one the `WHERE` clause bounds is used if one exists,
//! and the ordinary scan is used otherwise. There is no cost comparison
//! between candidate indexes (the first match wins) and no selectivity
//! estimate deciding whether the seek beats the scan at all -- both
//! need `planner::Stats`, which db-core has no equivalent of yet.
//!
//! sqlite-rs's `LIKE`/`GLOB` prefix seek and `IN (...)` list seek are
//! not ported: db-core's scoped-down [`Expr`] has neither construct.
//! Its `BETWEEN` seek is ported as the `col >= lo AND col <= hi` shape
//! db-core's `Expr` spells that with, and the descending-only bound
//! (`col < lit` alone) still falls back to the ordinary scan, exactly
//! as in the reference -- walking it needs a backward stop-check the
//! `Idx*` opcodes don't offer.

use super::index_scan::open_index_cursor;
use super::limit_scan::LimitState;
use super::select::emit_row;
use super::{Emitter, Instruction, Label, Opcode, RegAlloc, Result, Scope, TableSchema};
use crate::expr::{BinOp, Expr, Query};
use crate::types::Literal;
use crate::vm::row::{affinity_of, Affinity, Collation, SortKeyColumn, P4};

/// The column name a `WHERE`-clause operand names, or `None` for
/// anything that isn't a bare (unqualified) column reference.
pub(super) fn where_col(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Column(name) if !name.contains('.') => Some(name.as_str()),
        _ => None,
    }
}

/// Whether `expr` is an operand a seek bound can be built from -- a
/// literal, which is db-core's whole constant vocabulary (sqlite-rs also
/// accepts a bind parameter, which this crate's `Expr` has no variant
/// for).
pub(super) fn is_supported_operand(expr: &Expr) -> bool {
    matches!(expr, Expr::Literal(_))
}

/// The declared affinity of `col_name`, or `Blob` when the column has no
/// declared type.
fn column_affinity(schema: &TableSchema, col_name: &str) -> Affinity {
    schema
        .column_index(col_name)
        .and_then(|idx| schema.column_types.get(idx))
        .map_or(Affinity::Blob, |t| affinity_of(t))
}

/// Whether comparing `expr` against a column of `column_affinity` needs
/// no coercion -- the index key stores affinity-applied values, so a
/// bound that would be coerced can't be compared against raw key bytes
/// and must fall back to the ordinary scan.
pub(super) fn operand_matches_column_affinity(expr: &Expr, column_affinity: Affinity) -> bool {
    let Expr::Literal(lit) = expr else {
        return false;
    };
    matches!(
        (lit, column_affinity),
        (
            Literal::Int(_),
            Affinity::Integer | Affinity::Numeric | Affinity::Real
        ) | (Literal::Float(_), Affinity::Real | Affinity::Numeric)
            | (Literal::Str(_), Affinity::Text)
    )
}

/// The `schema.indexes` position of an index whose *leading* column is
/// `col_name`. Only that leading column is ever probed by the opcodes
/// these fast paths emit, so a multi-column index still works (just as a
/// leading-column-only lookup).
pub(super) fn find_leading_index(schema: &TableSchema, col_name: &str) -> Option<usize> {
    schema.indexes.iter().position(|index| {
        index
            .columns
            .first()
            .is_some_and(|c| c.eq_ignore_ascii_case(col_name))
    })
}

/// One end of a seek range: the column bounded, the literal bounding it,
/// and whether the bound itself qualifies.
struct Bound<'a> {
    column: &'a str,
    operand: &'a Expr,
    inclusive: bool,
}

/// `col > lit`/`col >= lit`/`lit < col`/`lit <= col` -- the shapes whose
/// low bound a forward `SeekIndexGE` walk can start from. The
/// descending-bound shapes (`col < lit` and friends) return `None`:
/// walking those forward from a low-bound seek would need a *backward*
/// walk from the top of the index, which needs a stop-check opcode this
/// codegen doesn't have.
fn as_lower_bound(expr: &Expr) -> Option<Bound<'_>> {
    let Expr::BinaryOp(lhs, op, rhs) = expr else {
        return None;
    };
    let (column, operand, inclusive) = match op {
        BinOp::Gt => (where_col(lhs)?, rhs.as_ref(), false),
        BinOp::Ge => (where_col(lhs)?, rhs.as_ref(), true),
        BinOp::Lt => (where_col(rhs)?, lhs.as_ref(), false),
        BinOp::Le => (where_col(rhs)?, lhs.as_ref(), true),
        _ => return None,
    };
    Some(Bound {
        column,
        operand,
        inclusive,
    })
}

/// `col < lit`/`col <= lit`/`lit > col`/`lit >= col` -- the shapes that
/// can stop a forward walk.
fn as_upper_bound(expr: &Expr) -> Option<Bound<'_>> {
    let Expr::BinaryOp(lhs, op, rhs) = expr else {
        return None;
    };
    let (column, operand, inclusive) = match op {
        BinOp::Lt => (where_col(lhs)?, rhs.as_ref(), false),
        BinOp::Le => (where_col(lhs)?, rhs.as_ref(), true),
        BinOp::Gt => (where_col(rhs)?, lhs.as_ref(), false),
        BinOp::Ge => (where_col(rhs)?, lhs.as_ref(), true),
        _ => return None,
    };
    Some(Bound {
        column,
        operand,
        inclusive,
    })
}

/// The bounds a `WHERE` clause offers a forward index walk: a lower
/// bound alone (`col > lit`), a lower plus an upper one on the same
/// column (`col >= lo AND col <= hi` -- how db-core's `Expr` spells
/// sqlite-rs's `BETWEEN`), or an equality (both bounds, both inclusive,
/// the same literal).
fn as_seek_bounds(where_expr: &Expr) -> Option<(Bound<'_>, Option<Bound<'_>>)> {
    if let Expr::BinaryOp(lhs, BinOp::And, rhs) = where_expr {
        let lo = as_lower_bound(lhs)?;
        let hi = as_upper_bound(rhs)?;
        if !lo.column.eq_ignore_ascii_case(hi.column) {
            return None;
        }
        return Some((lo, Some(hi)));
    }
    if let Expr::BinaryOp(lhs, BinOp::Eq, rhs) = where_expr {
        let (column, operand) = match (where_col(lhs), where_col(rhs)) {
            (Some(column), _) => (column, rhs.as_ref()),
            (None, Some(column)) => (column, lhs.as_ref()),
            (None, None) => return None,
        };
        let bound = |operand| Bound {
            column,
            operand,
            inclusive: true,
        };
        return Some((bound(operand), Some(bound(operand))));
    }
    Some((as_lower_bound(where_expr)?, None))
}

/// The column [`try_compile_range_seek`] would seek on and the operator
/// spelling `EXPLAIN QUERY PLAN` reports it under (`"="` for an
/// equality, `">"` for every bounded shape -- real sqlite3 collapses
/// inclusive and exclusive into the same `(col>?)` wording), or `None`
/// when no fast path applies. Factored out so [`super::eqp`] can
/// inspect the decision without emitting anything.
pub(super) fn seek_detail<'a>(
    query: &'a Query,
    schema: &TableSchema,
) -> Option<(&'a str, &'static str)> {
    if query.distinct || !query.joins.is_empty() {
        return None;
    }
    let (lo, hi) = as_seek_bounds(query.where_clause.as_ref()?)?;
    if hi.as_ref().is_some_and(|b| !b.inclusive) {
        return None;
    }
    if !is_supported_operand(lo.operand) {
        return None;
    }
    let affinity = column_affinity(schema, lo.column);
    if !operand_matches_column_affinity(lo.operand, affinity) {
        return None;
    }
    find_leading_index(schema, lo.column)?;
    let is_equality = matches!(
        query.where_clause.as_ref()?,
        Expr::BinaryOp(_, BinOp::Eq, _)
    );
    Some((lo.column, if is_equality { "=" } else { ">" }))
}

/// Compiles a `WHERE`-bounded scan of an indexed column as a
/// `SeekIndexGE` + `IdxCompareGT`-guarded `IdxNext` walk. Returns
/// `Ok(true)` when this fast path was taken; `Ok(false)` leaves
/// `em`/`reg` untouched so the caller falls back to its ordinary
/// sequential scan.
#[allow(clippy::too_many_arguments)]
pub(super) fn try_compile_range_seek(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    query: &Query,
    scope: &Scope,
    columns: &[String],
    sort_key: Option<SortKeyColumn>,
    sorter_cursor: i32,
    table_cursor: i32,
    index_cursor: i32,
    limit: Option<LimitState>,
    end_label: Label,
) -> Result<bool> {
    if query.distinct || !query.joins.is_empty() {
        return Ok(false);
    }
    let Some(where_expr) = &query.where_clause else {
        return Ok(false);
    };
    let Some((lo, hi)) = as_seek_bounds(where_expr) else {
        return Ok(false);
    };
    // `IdxCompareGT` is the only forward stop-check available, so only an
    // *inclusive* upper bound can be expressed (stop once the entry is
    // strictly past it). An exclusive one would need a `>=` stop-check
    // the `Idx*` opcodes don't offer, so it falls back to the ordinary
    // scan -- the same conservative fallback the reference applies to a
    // descending-only bound.
    if hi.as_ref().is_some_and(|b| !b.inclusive) {
        return Ok(false);
    }
    if !is_supported_operand(lo.operand)
        || hi
            .as_ref()
            .is_some_and(|b| !is_supported_operand(b.operand))
    {
        return Ok(false);
    }
    let affinity = column_affinity(&scope.schema, lo.column);
    if !operand_matches_column_affinity(lo.operand, affinity)
        || hi
            .as_ref()
            .is_some_and(|b| !operand_matches_column_affinity(b.operand, affinity))
    {
        return Ok(false);
    }
    let Some(index_position) = find_leading_index(&scope.schema, lo.column) else {
        return Ok(false);
    };
    let Some(index) = scope.schema.indexes.get(index_position) else {
        return Ok(false);
    };
    let key_p4 = P4::SeekKey(vec![Collation::Binary]);

    open_index_cursor(em, index, index_cursor)?;
    let lo_reg = super::compile_value(em, reg, scope, lo.operand)?;
    let hi_reg = match &hi {
        Some(b) => Some(super::compile_value(em, reg, scope, b.operand)?),
        None => None,
    };

    let seek_addr = em.emit(Instruction::with_p4(
        Opcode::SeekIndexGE,
        index_cursor,
        0,
        lo_reg,
        key_p4.clone(),
    ));
    em.patch_p2(seek_addr, end_label);

    if !lo.inclusive {
        // `SeekIndexGE`'s floor is inclusive, so a leading run of entries
        // equal to the bound (duplicate keys) needs skipping before the
        // walk below can treat "landed here" as "strictly past it".
        let skip_start = em.new_label();
        em.place(skip_start);
        let past_bound = em.new_label();
        let gt_addr = em.emit(Instruction::with_p4(
            Opcode::IdxCompareGT,
            index_cursor,
            0,
            lo_reg,
            key_p4.clone(),
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

    if let Some(hi_reg) = hi_reg {
        let stop_addr = em.emit(Instruction::with_p4(
            Opcode::IdxCompareGT,
            index_cursor,
            0,
            hi_reg,
            key_p4.clone(),
        ));
        em.patch_p2(stop_addr, end_label);
    }

    let row_skip = em.new_label();
    let rowid_reg = reg.alloc();
    em.emit(Instruction::new(
        Opcode::IdxRowid,
        index_cursor,
        rowid_reg,
        0,
    ));
    let table_seek_addr = em.emit(Instruction::new(
        Opcode::SeekRowid,
        table_cursor,
        0,
        rowid_reg,
    ));
    em.patch_p2(table_seek_addr, row_skip);

    emit_row(
        em,
        reg,
        scope,
        columns,
        None,
        sort_key,
        sorter_cursor,
        limit,
        row_skip,
        end_label,
    )?;

    em.place(row_skip);
    let next_addr = em.emit(Instruction::new(Opcode::IdxNext, index_cursor, 0, 0));
    em.patch_p2(next_addr, loop_start);
    Ok(true)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::codegen::row::IndexSchema;

    fn schema() -> TableSchema {
        TableSchema {
            name: "t".to_string(),
            columns: vec!["a".to_string(), "b".to_string()],
            column_types: vec!["INTEGER".to_string(), "TEXT".to_string()],
            rowid_alias: None,
            root_page: 2,
            indexes: vec![IndexSchema {
                name: "t_a".to_string(),
                root_page: 3,
                columns: vec!["a".to_string()],
            }],
        }
    }

    fn col(name: &str) -> Expr {
        Expr::Column(name.to_string())
    }

    fn int(n: i64) -> Expr {
        Expr::Literal(Literal::Int(n))
    }

    fn binary(lhs: Expr, op: BinOp, rhs: Expr) -> Expr {
        Expr::BinaryOp(Box::new(lhs), op, Box::new(rhs))
    }

    #[test]
    fn leading_index_is_found_case_insensitively() {
        assert_eq!(find_leading_index(&schema(), "A"), Some(0));
        assert_eq!(find_leading_index(&schema(), "b"), None);
    }

    #[test]
    fn forward_comparison_yields_a_lower_bound_only() {
        let expr = binary(col("a"), BinOp::Gt, int(5));
        let (lo, hi) = as_seek_bounds(&expr).unwrap();
        assert_eq!(lo.column, "a");
        assert!(!lo.inclusive);
        assert!(hi.is_none());
    }

    #[test]
    fn reversed_comparison_binds_the_column_side() {
        let expr = binary(int(5), BinOp::Le, col("a"));
        let (lo, _) = as_seek_bounds(&expr).unwrap();
        assert_eq!(lo.column, "a");
        assert!(lo.inclusive);
    }

    #[test]
    fn conjunction_of_two_bounds_yields_a_range() {
        let expr = binary(
            binary(col("a"), BinOp::Ge, int(1)),
            BinOp::And,
            binary(col("a"), BinOp::Le, int(9)),
        );
        let (lo, hi) = as_seek_bounds(&expr).unwrap();
        assert!(lo.inclusive);
        let hi = hi.unwrap();
        assert_eq!(hi.column, "a");
        assert!(hi.inclusive);
    }

    #[test]
    fn conjunction_over_two_different_columns_is_rejected() {
        let expr = binary(
            binary(col("a"), BinOp::Ge, int(1)),
            BinOp::And,
            binary(col("b"), BinOp::Le, int(9)),
        );
        assert!(as_seek_bounds(&expr).is_none());
    }

    #[test]
    fn equality_yields_both_bounds_inclusive() {
        let expr = binary(col("a"), BinOp::Eq, int(4));
        let (lo, hi) = as_seek_bounds(&expr).unwrap();
        assert!(lo.inclusive);
        assert!(hi.unwrap().inclusive);
    }

    #[test]
    fn a_text_bound_against_an_integer_column_is_rejected() {
        let schema = schema();
        let affinity = column_affinity(&schema, "a");
        assert!(operand_matches_column_affinity(&int(1), affinity));
        assert!(!operand_matches_column_affinity(
            &Expr::Literal(Literal::Str("x".to_string())),
            affinity
        ));
    }
}
