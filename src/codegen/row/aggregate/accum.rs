//! Aggregate accumulation primitives -- ported from sqlite-rs's
//! `codegen/select/aggregate/accum.rs`, scoped to db-core's narrower
//! [`SelectItem`]/[`Expr`] (see [`super`]'s module doc).

use super::super::value::emit_column_read;
use super::super::{
    CodegenError, CondTargets, Emitter, Label, RegAlloc, Result, Scope, TableSchema, Target,
};
use crate::parser::ast::{Expr, ExprKind, FunctionArgs, ResultColumn, Select};
use crate::vm::batch::AggFunc;
use crate::vm::row::{Collation, Instruction, Opcode, P4};

/// One aggregate call's `AggStep`/`AggFinal` binding: `func` selects the
/// accumulator kind in [`crate::vm::row::aggregate`], `arg` is its
/// single argument column (`None` only for `COUNT(*)`), and `slot` is
/// this call's aggregate-context slot number -- `Vm::agg_contexts`'
/// index, a table disjoint from the register file.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct AggSlot {
    pub(super) func: AggFunc,
    pub(super) arg: Option<String>,
    pub(super) slot: i32,
    /// The `SELECT`-item label this call is referred to by -- the field
    /// name it takes in [`flush_group`]'s synthetic per-group record,
    /// and the name a `HAVING` clause references it under.
    pub(super) label: String,
}

/// The label one aggregate call is known by, byte-identical to
/// `codegen::batch::select_item_label`'s rendering of the same call so a
/// `HAVING`/`ORDER BY` reference resolves against it (#131's convention).
pub(super) fn agg_label(func: AggFunc, arg: Option<&str>) -> String {
    format!("{}({})", func.name(), arg.unwrap_or("*"))
}

/// The inverse of [`agg_label`]: recognizes `"COUNT(*)"`/`"SUM(amount)"`
/// as an aggregate reference. db-core's [`Expr`] has no function-call
/// variant (unlike sqlite-rs's `ExprKind::FunctionCall`, which
/// `find_aggregates` matches on there), so an aggregate reached from a
/// `HAVING` clause arrives as an ordinary `Expr::Column` carrying this
/// label, and is recognized by parsing it back.
pub(super) fn parse_agg_label(name: &str) -> Option<(AggFunc, Option<String>)> {
    let open = name.find('(')?;
    let (head, rest) = name.split_at(open);
    let inner = rest.strip_prefix('(')?.strip_suffix(')')?;
    let func = AggFunc::from_name(head)?;
    if inner == "*" {
        return Some((func, None));
    }
    if inner.is_empty() {
        return None;
    }
    Some((func, Some(inner.to_string())))
}

/// Every aggregate call `query` accumulates: its result columns first,
/// then any additional call only its `HAVING` clause references --
/// deduplicated so a `HAVING COUNT(*) > 1` sharing a call with a
/// `COUNT(*)` result column accumulates into one slot, exactly as
/// sqlite-rs's `collect_aggregates` does.
pub(super) fn collect_aggregates(query: &Select) -> Result<Vec<AggSlot>> {
    let mut found: Vec<(AggFunc, Option<String>)> = Vec::new();
    for item in &query.columns {
        let ResultColumn::Expr { expr, .. } = item else {
            continue;
        };
        if let ExprKind::FunctionCall { over: Some(_), .. } = &expr.kind {
            return Err(CodegenError::Unsupported {
                reason: "window functions are not supported by codegen::row".to_string(),
            });
        }
        if let Some(entry) = as_aggregate(expr)? {
            if !found.contains(&entry) {
                found.push(entry);
            }
        }
    }
    if let Some(having) = &query.having {
        find_aggregates(having, &mut found);
    }
    Ok(found
        .into_iter()
        .enumerate()
        .map(|(slot, (func, arg))| AggSlot {
            label: agg_label(func, arg.as_deref()),
            func,
            arg,
            slot: i32::try_from(slot).unwrap_or(0),
        })
        .collect())
}

/// Whether `query` aggregates at all -- the trigger for compiling an
/// implicit whole-table group when `query.group_by` is empty,
/// distinguishing `SELECT COUNT(*) FROM t` from an ordinary
/// aggregate-free `SELECT` (a plain scan).
pub(crate) fn query_has_aggregate(query: &Select) -> bool {
    let has_agg_column = query.columns.iter().any(|item| match item {
        ResultColumn::Expr { expr, .. } => matches!(as_aggregate(expr), Ok(Some(_))),
        ResultColumn::Star | ResultColumn::TableStar { .. } => false,
    });
    if has_agg_column {
        return true;
    }
    let mut found = Vec::new();
    if let Some(having) = &query.having {
        find_aggregates(having, &mut found);
    }
    !found.is_empty()
}

/// The aggregate call `expr` is, or `None` for any other expression.
///
/// `expr::SelectItem` had a dedicated `Agg(AggFunc, Option<String>)`
/// variant, so the parser decided what counted as an aggregate. The AST
/// spells every call the same way -- `ExprKind::FunctionCall` -- and
/// leaves the classification to whoever compiles it (#147), which is
/// here. `COUNT(*)` carries `FunctionArgs::Star` and maps to `None`,
/// matching the old variant's `Option<String>` argument.
///
/// A call whose name *is* an aggregate but whose argument this planner
/// cannot accumulate (an expression rather than a bare column, or more
/// than one argument) is an error rather than `None`: silently treating
/// `SUM(a + b)` as a non-aggregate would compile it as a plain column
/// and answer nonsense.
pub(super) fn as_aggregate(expr: &Expr) -> Result<Option<(AggFunc, Option<String>)>> {
    let ExprKind::FunctionCall {
        name,
        args,
        over: None,
        ..
    } = &expr.kind
    else {
        return Ok(None);
    };
    let Some(func) = AggFunc::from_name(name) else {
        return Ok(None);
    };
    match args {
        FunctionArgs::Star => Ok(Some((func, None))),
        FunctionArgs::List(args) => match args.as_slice() {
            [] => Ok(Some((func, None))),
            [arg] => match &arg.kind {
                ExprKind::Column {
                    table: None, name, ..
                } => Ok(Some((func, Some(name.clone())))),
                // A qualified reference (`SUM(u.c)`) keeps its
                // qualifier -- downstream column resolution needs the
                // full "table.column" spelling, the same string
                // `expr::SelectItem::Agg`'s caller-supplied argument
                // always carried.
                ExprKind::Column {
                    table: Some(table),
                    name,
                    ..
                } => Ok(Some((func, Some(format!("{table}.{name}"))))),
                _ => Err(CodegenError::Unsupported {
                    reason: format!(
                        "{name} over an expression is not supported by codegen::row yet"
                    ),
                }),
            },
            _ => Err(CodegenError::Unsupported {
                reason: format!("{name} with more than one argument is not supported"),
            }),
        },
    }
}

/// Aggregate calls reachable from `expr`, by the label a compiled
/// aggregate is bound to. A `HAVING` clause names an aggregate by
/// referring to the output column it produced, so these arrive as
/// column references carrying [`agg_label`]'s spelling, not as calls.
fn find_aggregates(expr: &Expr, out: &mut Vec<(AggFunc, Option<String>)>) {
    super::super::walk_columns(expr, &mut |name| {
        if let Some(entry) = parse_agg_label(name) {
            if !out.contains(&entry) {
                out.push(entry);
            }
        }
    });
}

/// Emits one `AggStep` for `agg`'s slot: reads `agg.arg` (if any) into a
/// fresh register and folds it, exactly the shape
/// [`crate::vm::row`]'s `AggStep` dispatch expects -- a contiguous
/// argument-register run starting at `p2`, name/arity/collation via
/// [`P4::AggFunc`]. `reset` sets `p5`, discarding this slot's prior
/// state before folding: a group's boundary row passes `true` so a slot
/// number reused from the previous group starts a fresh accumulator,
/// every later row in the same group passes `false`.
///
/// `min`/`max` compare under `p4`'s collation. db-core's
/// [`TableSchema`] carries no per-column `COLLATE` and its [`Expr`] has
/// no `COLLATE` wrapper, so that is always [`Collation::Binary`] here --
/// the operand is threaded through regardless, so the day either grows
/// one this is the only place that changes.
pub(super) fn emit_agg_step(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    scope: &Scope,
    agg: &AggSlot,
    reset: bool,
) -> Result<()> {
    let (arg_reg, arity) = match &agg.arg {
        Some(name) => (
            Some(super::super::compile_value(
                em,
                reg,
                scope,
                &super::super::column_expr(name.clone()),
            )?),
            1usize,
        ),
        None => (None, 0usize),
    };
    let mut instr = Instruction::with_p4(
        Opcode::AggStep,
        agg.slot,
        arg_reg.unwrap_or(0),
        0,
        P4::AggFunc {
            name: agg.func.name().to_ascii_lowercase(),
            arity,
            collation: Collation::Binary,
        },
    );
    if reset {
        instr.p5 = 1;
    }
    em.emit(instr);
    Ok(())
}

/// A pseudo cursor re-reads an already-materialized record, so the
/// rowid-alias column is an ordinary field within it rather than
/// something `Opcode::Rowid` can fetch -- see
/// [`super::super::value::emit_column_read`]'s doc for the distinction.
pub(super) fn read_pseudo_column(
    em: &mut Emitter,
    cursor: i32,
    idx: usize,
    dest: i32,
) -> Result<()> {
    em.emit(Instruction::new(
        Opcode::Column,
        cursor,
        column_operand(idx)?,
        dest,
    ));
    Ok(())
}

/// Reads every column of a pseudo cursor's record into the given
/// (already-allocated, persistent) registers -- the per-group snapshot a
/// plain (non-aggregate) result/`HAVING` column reads, matching SQLite's
/// "arbitrary row" semantics for a non-grouped-by column.
pub(super) fn read_row_columns_into(em: &mut Emitter, cursor: i32, dest: &[i32]) -> Result<()> {
    for (idx, &r) in dest.iter().enumerate() {
        read_pseudo_column(em, cursor, idx, r)?;
    }
    Ok(())
}

pub(super) fn column_operand(idx: usize) -> Result<i32> {
    i32::try_from(idx).map_err(|_| CodegenError::Unsupported {
        reason: format!("column index {idx} does not fit in a p2 operand"),
    })
}

pub(super) fn count_operand(count: usize) -> Result<i32> {
    i32::try_from(count).map_err(|_| CodegenError::Unsupported {
        reason: format!("a run of {count} registers does not fit in a p2 operand"),
    })
}

/// A synthetic single-table schema over one flushed group's record:
/// `columns`' values (the group's snapshot row) followed by one field
/// per aggregate, named by its label -- so `query.columns`/`query.having`
/// compile against it through the ordinary, aggregate-unaware
/// `compile_value`/`compile_cond` machinery.
fn synthetic_schema(columns: &[String], agg_slots: &[AggSlot]) -> TableSchema {
    let mut names = columns.to_vec();
    names.extend(agg_slots.iter().map(|a| a.label.clone()));
    TableSchema {
        name: String::new(),
        column_types: names.iter().map(|_| String::new()).collect(),
        columns: names,
        rowid_alias: None,
        root_page: 0,
        indexes: vec![],
    }
}

/// Emits one grouped output row: builds a synthetic record (the group's
/// snapshot column values followed by each aggregate's finalized value),
/// opens a pseudo cursor over it, applies `HAVING` and the `LIMIT`
/// guard, then projects `query`'s result columns through `sink`.
///
/// Structured exactly as sqlite-rs's `flush_group`, including its reason
/// for the detour through a record: it is what lets `HAVING` and the
/// result-column projection be compiled by the same code every
/// non-aggregate scan uses, with no aggregate-awareness anywhere in it.
#[allow(clippy::too_many_arguments)]
pub(super) fn flush_group<F>(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    query: &Select,
    columns: &[String],
    snapshot_regs: &[i32],
    agg_slots: &[AggSlot],
    limit_reg: Option<i32>,
    flush_cursor: i32,
    end_label: Label,
    sink: &mut F,
) -> Result<()>
where
    F: FnMut(&mut Emitter, &mut RegAlloc, i32, usize) -> Result<()>,
{
    let schema = synthetic_schema(columns, agg_slots);

    // Bump-allocated up front and in one run, so `dests` is contiguous
    // for `MakeRecord` by construction.
    let synthetic_count = snapshot_regs.len().saturating_add(agg_slots.len());
    let dests: Vec<i32> = (0..synthetic_count).map(|_| reg.alloc()).collect();
    let synthetic_first = dests.first().copied().unwrap_or_else(|| reg.alloc());
    for (&snap, &dest) in snapshot_regs.iter().zip(&dests) {
        em.emit(Instruction::new(Opcode::Copy, snap, dest, 0));
    }
    let agg_dests = dests.get(snapshot_regs.len()..).unwrap_or(&[]);
    for (agg, &dest) in agg_slots.iter().zip(agg_dests) {
        emit_agg_final(em, agg, dest);
    }

    let record_reg = reg.alloc();
    em.emit(Instruction::new(
        Opcode::MakeRecord,
        synthetic_first,
        count_operand(synthetic_count)?,
        record_reg,
    ));
    em.emit(Instruction::new(
        Opcode::OpenPseudo,
        flush_cursor,
        record_reg,
        0,
    ));

    let scope = Scope::single(schema.clone(), flush_cursor);
    let skip_label = em.new_label();
    if let Some(having) = &query.having {
        super::super::compile_cond(
            em,
            reg,
            &scope,
            having,
            CondTargets::null_is_false(Target::Fallthrough, Target::Jump(skip_label)),
        )?;
    }
    if let Some(limit_reg) = limit_reg {
        super::super::select::emit_limit_guard(em, limit_reg, end_label);
    }

    let names = projected_names(query, columns)?;
    let (first, count) = project(em, reg, &schema, flush_cursor, &names)?;
    sink(em, reg, first, count)?;
    em.place(skip_label);
    Ok(())
}

/// `AggFinal` reads slot `agg.slot`'s finalized value into `dest`.
/// `avg()`'s sum/count division happens inside
/// [`crate::vm::row::aggregate::finalize`], so this is a plain read.
pub(super) fn emit_agg_final(em: &mut Emitter, agg: &AggSlot, dest: i32) {
    let arity = usize::from(agg.arg.is_some());
    em.emit(Instruction::with_p4(
        Opcode::AggFinal,
        agg.slot,
        0,
        dest,
        P4::Str(format!("{}({arity})", agg.func.name().to_ascii_lowercase())),
    ));
}

/// `query`'s result columns as names within the flushed group's
/// synthetic record: a bare column verbatim, `*` expanded to `columns`,
/// an aggregate call to its [`agg_label`].
pub(super) fn projected_names(query: &Select, columns: &[String]) -> Result<Vec<String>> {
    let mut names = Vec::with_capacity(query.columns.len());
    for item in &query.columns {
        match item {
            ResultColumn::Star => names.extend(columns.iter().cloned()),
            ResultColumn::TableStar { table } => {
                return Err(CodegenError::Unsupported {
                    reason: format!("`{table}.*` is not supported yet"),
                })
            }
            ResultColumn::Expr { expr, .. } => {
                if let ExprKind::FunctionCall { over: Some(_), .. } = &expr.kind {
                    return Err(CodegenError::Unsupported {
                        reason: "window functions are not supported by codegen::row".to_string(),
                    });
                }
                match as_aggregate(expr)? {
                    Some((func, arg)) => names.push(agg_label(func, arg.as_deref())),
                    None => match &expr.kind {
                        ExprKind::Column { name, .. } => names.push(name.clone()),
                        _ => {
                            return Err(CodegenError::Unsupported {
                                reason: "a computed result column is not supported by \
                                         codegen::row yet"
                                    .to_string(),
                            })
                        }
                    },
                }
            }
        }
    }
    Ok(names)
}

/// Reads `names` off `cursor` into a contiguous register run, the
/// `(first, count)` window `ResultRow` consumes. Registers are freshly
/// bump-allocated and therefore already adjacent; the check mirrors
/// `select::compile_row_values`' defensive one rather than assuming so.
fn project(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    schema: &TableSchema,
    cursor: i32,
    names: &[String],
) -> Result<(i32, usize)> {
    if names.is_empty() {
        return Ok((reg.alloc(), 0));
    }
    let mut regs = Vec::with_capacity(names.len());
    for name in names {
        let (_, idx) = Scope::single(schema.clone(), cursor).resolve(name)?;
        let r = reg.alloc();
        emit_column_read(em, schema, cursor, idx, r)?;
        regs.push(r);
    }
    let first = regs.first().copied().unwrap_or(0);
    let contiguous = regs
        .iter()
        .enumerate()
        .all(|(i, &r)| r == first.saturating_add(i32::try_from(i).unwrap_or(i32::MAX)));
    if contiguous {
        return Ok((first, regs.len()));
    }
    let dests: Vec<i32> = (0..regs.len()).map(|_| reg.alloc()).collect();
    for (&r, &dest) in regs.iter().zip(&dests) {
        em.emit(Instruction::new(Opcode::Copy, r, dest, 0));
    }
    Ok((dests.first().copied().unwrap_or(0), dests.len()))
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

    #[test]
    fn agg_label_round_trips_through_parse_agg_label() {
        for (func, arg) in [
            (AggFunc::Count, None),
            (AggFunc::Sum, Some("amount")),
            (AggFunc::Avg, Some("t.amount")),
            (AggFunc::Min, Some("a")),
            (AggFunc::Max, Some("a")),
        ] {
            let label = agg_label(func, arg);
            assert_eq!(
                parse_agg_label(&label),
                Some((func, arg.map(str::to_string))),
                "label = {label}"
            );
        }
    }

    #[test]
    fn parse_agg_label_rejects_a_plain_column_name() {
        assert_eq!(parse_agg_label("amount"), None);
        assert_eq!(parse_agg_label("MEDIAN(a)"), None);
        assert_eq!(parse_agg_label("COUNT()"), None);
        assert_eq!(parse_agg_label("COUNT(a"), None);
    }

    #[test]
    fn collect_aggregates_dedups_a_having_call_shared_with_a_result_column() {
        // `HAVING` names the aggregate by the output column it
        // produced, which is why the two dedup to one slot.
        let query = crate::codegen::row::testutil::select(
            "SELECT COUNT(*) FROM t GROUP BY a HAVING \"COUNT(*)\" > 1",
        );
        let slots = collect_aggregates(&query).unwrap();
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].slot, 0);
        assert_eq!(slots[0].label, "COUNT(*)");
    }
}
