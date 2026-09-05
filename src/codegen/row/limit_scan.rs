//! `LIMIT`/`OFFSET` counter setup and the per-row guards every scan in
//! this module emits around an output row (db-core#94) -- ported from
//! sqlite-rs's `src/codegen/select/limit_scan.rs`.
//!
//! Only that file's LIMIT/OFFSET slice is ported here. Its skip-scan
//! (`find_skip_scan_index`/`try_compile_skip_scan_index`) and
//! `compile_direct_scan` dispatcher both take a `planner::Stats`
//! argument -- `is_skip_scan_worthwhile` is a pure cardinality
//! judgement over `sqlite_stat1` -- which db-core has no equivalent of
//! yet (see [`super`]'s note on `planner.rs`, deferred with #116/#117),
//! so those stay unported rather than being stubbed with a fabricated
//! cost.

use super::{CodegenError, Emitter, Instruction, Label, Opcode, RegAlloc, Result};
use crate::expr::Query;

/// The `LIMIT`/`OFFSET` counter registers, set up once before a scan
/// loop starts. Mirrors sqlite-rs's own `LimitState`, holding plain
/// register numbers rather than compiled `Expr`s since db-core's
/// [`Query::limit`]/[`Query::offset`] are already plain values.
#[derive(Debug, Clone, Copy)]
pub(super) struct LimitState {
    pub limit_reg: Option<i32>,
    pub offset_reg: Option<i32>,
}

/// Emits the `LIMIT`/`OFFSET` counter registers, or `None` when the
/// query has neither.
pub(super) fn compile_limit_setup(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    query: &Query,
) -> Result<Option<LimitState>> {
    if query.limit.is_none() && query.offset.is_none() {
        return Ok(None);
    }
    let limit_reg = match query.limit {
        Some(limit) => Some(emit_counter(em, reg, limit, "LIMIT")?),
        None => None,
    };
    let offset_reg = match query.offset {
        Some(offset) => Some(emit_counter(em, reg, offset, "OFFSET")?),
        None => None,
    };
    Ok(Some(LimitState {
        limit_reg,
        offset_reg,
    }))
}

fn emit_counter(em: &mut Emitter, reg: &mut RegAlloc, value: usize, what: &str) -> Result<i32> {
    let p1 = i32::try_from(value).map_err(|_| CodegenError::Unsupported {
        reason: format!("{what} {value} does not fit in a p1 operand"),
    })?;
    let r = reg.alloc();
    em.emit(Instruction::new(Opcode::Integer, p1, r, 0));
    Ok(r)
}

/// Emits the `OFFSET` skip-guard (jumping to `row_skip` while
/// `offset_reg` still has rows to skip) -- called once per qualifying
/// row, before deciding whether to emit it. `IfPos`'s `p3` decrements
/// the register on the taken branch, so the guard stops firing once the
/// requested number of rows has been skipped.
pub(super) fn emit_offset_guard(em: &mut Emitter, limit: &LimitState, row_skip: Label) {
    if let Some(offset_reg) = limit.offset_reg {
        let addr = em.emit(Instruction::new(Opcode::IfPos, offset_reg, 0, 1));
        em.patch_p2(addr, row_skip);
    }
}

/// Emits the `LIMIT` stop-guard -- see [`super::select::emit_limit_guard`]
/// for why it checks before, rather than after, emitting the row.
pub(super) fn emit_limit_guard(em: &mut Emitter, limit: &LimitState, end_label: Label) {
    if let Some(limit_reg) = limit.limit_reg {
        super::select::emit_limit_guard(em, limit_reg, end_label);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::expr::{Query, SelectItem};

    fn query(limit: Option<usize>, offset: Option<usize>) -> Query {
        Query {
            columns: vec![SelectItem::Column("a".to_string())],
            from: "t".into(),
            joins: vec![],
            where_clause: None,
            distinct: false,
            group_by: vec![],
            having: None,
            order_by: None,
            limit,
            offset,
        }
    }

    #[test]
    fn no_limit_or_offset_sets_up_nothing() {
        let mut em = Emitter::new();
        let mut reg = RegAlloc::new();
        assert!(compile_limit_setup(&mut em, &mut reg, &query(None, None))
            .unwrap()
            .is_none());
        assert!(em.finish().instructions.is_empty());
    }

    #[test]
    fn offset_alone_sets_up_only_the_offset_counter() {
        let mut em = Emitter::new();
        let mut reg = RegAlloc::new();
        let state = compile_limit_setup(&mut em, &mut reg, &query(None, Some(7)))
            .unwrap()
            .unwrap();
        assert!(state.limit_reg.is_none());
        let program = em.finish();
        assert_eq!(program.instructions.len(), 1);
        assert_eq!(program.instructions[0].opcode, Opcode::Integer);
        assert_eq!(program.instructions[0].p1, 7);
    }

    #[test]
    fn offset_guard_decrements_and_skips() {
        let mut em = Emitter::new();
        let mut reg = RegAlloc::new();
        let state = compile_limit_setup(&mut em, &mut reg, &query(Some(3), Some(2)))
            .unwrap()
            .unwrap();
        let row_skip = em.new_label();
        emit_offset_guard(&mut em, &state, row_skip);
        em.place(row_skip);
        let program = em.finish();
        let guard = program
            .instructions
            .iter()
            .find(|i| i.opcode == Opcode::IfPos)
            .unwrap();
        assert_eq!(guard.p1, state.offset_reg.unwrap());
        assert_eq!(guard.p3, 1);
    }
}
