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

use super::{Emitter, Instruction, Label, Opcode, RegAlloc, Result, Scope};
use crate::parser::ast::Select;

/// The `LIMIT`/`OFFSET` counter registers, set up once before a scan
/// loop starts. Mirrors sqlite-rs's own `LimitState`, holding plain
/// register numbers rather than compiled `Expr`s. The AST spells
/// `LIMIT`/`OFFSET` as expressions ([`crate::parser::ast::Limit`]);
/// any expression `compile_value` supports is compiled straight into
/// the counter register (db-core#149) -- integer literals still emit
/// a bare `Opcode::Integer` immediate since that's what `compile_value`
/// itself emits for them.
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
    scope: &Scope,
    query: &Select,
) -> Result<Option<LimitState>> {
    // The AST nests `OFFSET` inside `LIMIT` (SQLite's grammar has no
    // bare `OFFSET`), so one `None` check covers both.
    let Some(limit) = &query.limit else {
        return Ok(None);
    };
    let limit_reg = Some(super::value::compile_value(em, reg, scope, &limit.limit)?);
    let offset_reg = match &limit.offset {
        Some(offset) => Some(super::value::compile_value(em, reg, scope, offset)?),
        None => None,
    };
    Ok(Some(LimitState {
        limit_reg,
        offset_reg,
    }))
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
    use crate::codegen::row::testutil::select;
    use crate::codegen::row::TableSchema;

    fn scope() -> Scope {
        Scope::single(
            TableSchema {
                name: "t".into(),
                columns: vec!["a".into()],
                column_types: vec![String::new()],
                rowid_alias: None,
                root_page: 0,
                indexes: vec![],
                ..Default::default()
            },
            0,
        )
    }

    #[test]
    fn no_limit_sets_up_nothing() {
        let mut em = Emitter::new();
        let mut reg = RegAlloc::new();
        assert!(
            compile_limit_setup(&mut em, &mut reg, &scope(), &select("SELECT a FROM t"))
                .unwrap()
                .is_none()
        );
        assert!(em.finish().instructions.is_empty());
    }

    /// SQLite's grammar has no bare `OFFSET` -- it is only reachable as
    /// `LIMIT n OFFSET m`, which the AST records by nesting `offset`
    /// inside `Limit`. `expr::Query` had them as independent
    /// `Option<usize>` fields, so the old suite tested an
    /// offset-without-limit query the parser could never produce.
    #[test]
    fn limit_and_offset_each_get_a_counter() {
        let mut em = Emitter::new();
        let mut reg = RegAlloc::new();
        let state = compile_limit_setup(
            &mut em,
            &mut reg,
            &scope(),
            &select("SELECT a FROM t LIMIT 3 OFFSET 7"),
        )
        .unwrap()
        .unwrap();
        assert!(state.limit_reg.is_some());
        assert!(state.offset_reg.is_some());
        let program = em.finish();
        let counters: Vec<i32> = program
            .instructions
            .iter()
            .filter(|i| i.opcode == Opcode::Integer)
            .map(|i| i.p1)
            .collect();
        assert_eq!(counters, vec![3, 7]);
    }

    #[test]
    fn limit_alone_sets_up_only_the_limit_counter() {
        let mut em = Emitter::new();
        let mut reg = RegAlloc::new();
        let state = compile_limit_setup(
            &mut em,
            &mut reg,
            &scope(),
            &select("SELECT a FROM t LIMIT 5"),
        )
        .unwrap()
        .unwrap();
        assert!(state.offset_reg.is_none());
        let program = em.finish();
        assert_eq!(program.instructions.len(), 1);
        assert_eq!(program.instructions[0].opcode, Opcode::Integer);
        assert_eq!(program.instructions[0].p1, 5);
    }

    /// The AST allows any expression as a `LIMIT`/`OFFSET` bound (`LIMIT
    /// n + 1`); it's compiled through the same expression compiler as
    /// `WHERE`/projections rather than requiring an integer literal
    /// (db-core#149).
    #[test]
    fn a_non_literal_limit_compiles_via_the_expression_compiler() {
        let mut em = Emitter::new();
        let mut reg = RegAlloc::new();
        let state = compile_limit_setup(
            &mut em,
            &mut reg,
            &scope(),
            &select("SELECT a FROM t LIMIT 2 + 3"),
        )
        .unwrap()
        .unwrap();
        assert!(state.limit_reg.is_some());
        let program = em.finish();
        assert!(program.instructions.iter().any(|i| i.opcode == Opcode::Add));
    }

    #[test]
    fn offset_guard_decrements_and_skips() {
        let mut em = Emitter::new();
        let mut reg = RegAlloc::new();
        let state = compile_limit_setup(
            &mut em,
            &mut reg,
            &scope(),
            &select("SELECT a FROM t LIMIT 3 OFFSET 2"),
        )
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
