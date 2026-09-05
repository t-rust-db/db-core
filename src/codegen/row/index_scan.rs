//! Index-ordered scans (db-core#94, ADR 0020) -- ported from sqlite-rs's
//! `src/codegen/select/index_scan.rs`.
//!
//! `SELECT ... ORDER BY <indexed col> [DESC]` compiles to a direct index
//! b-tree walk -- `IdxRewind`/`IdxNext` (forward) or `IdxLast`/`IdxPrev`
//! (backward), `IdxRowid` + `SeekRowid` to fetch the full row -- in
//! place of [`super::select`]'s `Rewind`/`Next` + sorter pipeline. No
//! buffering, no sort, ever.
//!
//! **Scoped to the single mechanically-decidable case**, mirroring
//! [`super`]'s own note on `planner.rs`: this path is taken when *an*
//! index's leading column satisfies the whole `ORDER BY`, and is not
//! taken at all when a `WHERE` clause is present -- with no `Stats`,
//! there is no way to judge whether walking the index still beats a
//! filtered sequential scan, and sqlite-rs's own MVP guardrail here is
//! the same "no `WHERE` at all" stand-in. Consequently there is no
//! multi-index cost comparison: the first matching index wins.
//! sqlite-rs's partial-prefix variant
//! (`try_compile_partial_sorted_index_scan`, its #574) is not ported --
//! db-core's [`crate::expr::OrderBy`] is a single term, so a strict
//! non-empty prefix of it can never exist.

use super::limit_scan::LimitState;
use super::select::emit_row;
use super::{
    CodegenError, Emitter, IndexSchema, Instruction, Label, Opcode, RegAlloc, Result, Scope,
    TableSchema,
};
use crate::expr::Query;

/// The `schema.indexes` position of an index whose leading column
/// satisfies `order_by`, plus whether producing that order needs a
/// forward (ascending b-tree order) or backward walk.
///
/// db-core's [`IndexSchema`] is ascending-only (no per-column `DESC`),
/// so unlike sqlite-rs's `index_matches_ordering` there is no
/// direction-agreement check across columns: a descending `ORDER BY` is
/// always the exact reverse of the index's own order, hence a backward
/// walk.
pub(super) fn find_ordering_index(schema: &TableSchema, order_by_column: &str) -> Option<usize> {
    schema.indexes.iter().position(|index| {
        index
            .columns
            .first()
            .is_some_and(|c| c.eq_ignore_ascii_case(order_by_column))
    })
}

/// An index's root page, rejected when it is 0 -- a real index b-tree
/// never lives on page 0 (that offset is the database header), so a 0
/// here means the schema was built without one.
pub(super) fn valid_index_root_page(index: &IndexSchema) -> Result<i32> {
    if index.root_page == 0 {
        return Err(CodegenError::Unsupported {
            reason: format!("index {} has no root page", index.name),
        });
    }
    i32::try_from(index.root_page).map_err(|_| CodegenError::Unsupported {
        reason: format!(
            "index {} root page does not fit in a p2 operand",
            index.name
        ),
    })
}

/// Opens `index` on `index_cursor` for reading. `p5 = 1` marks the
/// cursor as an index cursor, which is what the `Idx*` opcodes probe
/// for.
pub(super) fn open_index_cursor(
    em: &mut Emitter,
    index: &IndexSchema,
    index_cursor: i32,
) -> Result<()> {
    let root_page = valid_index_root_page(index)?;
    let mut open_instr = Instruction::new(Opcode::OpenRead, index_cursor, root_page, 0);
    open_instr.p5 = 1;
    em.emit(open_instr);
    Ok(())
}

/// Compiles `SELECT ... ORDER BY <indexed col> [DESC] [LIMIT n [OFFSET
/// m]]` as a direct index walk. Returns `Ok(true)` when this fast path
/// was taken; `Ok(false)` leaves `em`/`reg` untouched so the caller
/// falls back to its ordinary sorted scan.
#[allow(clippy::too_many_arguments)]
pub(super) fn try_compile_index_ordered_scan(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    query: &Query,
    scope: &Scope,
    columns: &[String],
    table_cursor: i32,
    index_cursor: i32,
    limit: Option<LimitState>,
    end_label: Label,
) -> Result<bool> {
    if query.where_clause.is_some() || query.distinct || !query.joins.is_empty() {
        return Ok(false);
    }
    let Some(order_by) = &query.order_by else {
        return Ok(false);
    };
    let Some(index_position) = find_ordering_index(&scope.schema, &order_by.column) else {
        return Ok(false);
    };
    let Some(index) = scope.schema.indexes.get(index_position) else {
        return Ok(false);
    };
    open_index_cursor(em, index, index_cursor)?;

    let (rewind_op, next_op) = if order_by.descending {
        (Opcode::IdxLast, Opcode::IdxPrev)
    } else {
        (Opcode::IdxRewind, Opcode::IdxNext)
    };
    let rewind_addr = em.emit(Instruction::new(rewind_op, index_cursor, 0, 0));
    em.patch_p2(rewind_addr, end_label);

    let loop_start = em.new_label();
    em.place(loop_start);
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
        em, reg, scope, columns, None, None, 0, limit, row_skip, end_label,
    )?;

    em.place(row_skip);
    let next_addr = em.emit(Instruction::new(next_op, index_cursor, 0, 0));
    em.patch_p2(next_addr, loop_start);
    Ok(true)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn schema_with_index(index_columns: Vec<&str>, root_page: u32) -> TableSchema {
        TableSchema {
            name: "t".to_string(),
            columns: vec!["a".to_string(), "b".to_string()],
            column_types: vec!["INTEGER".to_string(), "TEXT".to_string()],
            rowid_alias: None,
            root_page: 2,
            indexes: vec![IndexSchema {
                name: "t_a".to_string(),
                root_page,
                columns: index_columns.into_iter().map(str::to_string).collect(),
            }],
        }
    }

    #[test]
    fn leading_column_match_is_found_case_insensitively() {
        let schema = schema_with_index(vec!["a"], 3);
        assert_eq!(find_ordering_index(&schema, "A"), Some(0));
        assert_eq!(find_ordering_index(&schema, "b"), None);
    }

    #[test]
    fn multi_column_index_matches_on_its_leading_column_only() {
        let schema = schema_with_index(vec!["a", "b"], 3);
        assert_eq!(find_ordering_index(&schema, "a"), Some(0));
        assert_eq!(find_ordering_index(&schema, "b"), None);
    }

    #[test]
    fn zero_root_page_is_rejected() {
        let schema = schema_with_index(vec!["a"], 0);
        assert!(valid_index_root_page(&schema.indexes[0]).is_err());
    }
}
