//! Columnar query planner: [`parser::ast::Select`] to an executable
//! [`Program`] for the batch executor -- what sqlite-rs's `src/codegen/*`
//! does for its VDBE (ADR 0007). Moved here from column-rs's `src/query.rs`
//! verbatim in behavior: nothing in this module ever touched Parquet, so
//! it never belonged in the storage glue.
//!
//! **Consumes `parser::ast::Select` directly (#153)** -- the same AST
//! [`crate::codegen::row`] consumes, not a private lowered type. The
//! caller is expected to hand this module a `Select` that already passed
//! [`crate::parser::column`]'s validator (the batch planner's analytics
//! subset -- one `FROM` table plus at most one equi-join, no `WITH`/
//! `UNION`/`HAVING`, a single `ORDER BY` term, ...): this module doesn't
//! re-run those checks, it plans. [`WindowFunc`]/[`WindowSpec`] are this
//! module's own local types (mirroring `ast::ExprKind::FunctionCall`'s
//! `OVER` tail), not shared with `parser`/`emit`.
//!
//! Four entry points, one per query shape the executor distinguishes:
//!
//! - [`compile`] -- flat/`GROUP BY`/`ORDER BY`/`LIMIT` single-table
//!   queries. The output [`Program`] always ends in [`Opcode::Combine`],
//!   optionally followed by `Sort`/`Limit` (db-core#48) -- together they
//!   carry the cross-segment merge/sort/limit metadata; the columns to
//!   load are derivable via [`Program::columns_to_load`]. No sidecar plan
//!   struct.
//! - [`compile_join`] -- one `INNER`/`LEFT` equi-join: build and probe
//!   programs plus the flat body over the joined batch
//!   ([`JoinProgram`], driven by [`crate::vm::engine::run_join`]).
//! - [`compile_semi_join`] -- `WHERE col IN (SELECT ...)`: the key column,
//!   the subquery (planned separately by the caller via [`compile`]) and
//!   the flat body over the filtered main table.
//! - [`compile_window`] -- `SELECT`s containing window functions: a flat
//!   program whose `Window` opcodes write one register per window item,
//!   ending in `Emit` + `Combine` (+ optional `Sort`/`Limit`) like any
//!   other flat program.
//!
//! Plus [`explain`], the `EXPLAIN` plan-tree construction over the same
//! planning decisions, and [`output_column_names`] for result headers.
//!
//! [`emit`] is the ahead-of-time Rust-source emitter for this planner's
//! output (db-core#192 -- folded in from the former crate-level `emit`
//! module; see ADR 0007's addendum there for why the standalone
//! `batch`/`row`/`stream` mirror of `emit` was retracted while its
//! vocabulary decision -- `codegen` = planner, `emit` = AOT renderer --
//! stands).

#[cfg(feature = "emit-batch")]
pub mod emit;
mod join_columns;
pub use join_columns::{resolve_join_columns, JoinColumnsError, ResolvedJoin};

use crate::parser::ast::{
    BinaryOp as AstBinOp, Distinctness, Expr as AstExpr, ExprKind, FromClause as AstFromClause,
    FunctionArgs, JoinConstraint, JoinOp, Literal as AstLiteral, ResultColumn, Select,
    TableRefKind,
};
use crate::vm::batch::{
    AggFunc, AggPart, Instruction, MapOp, Opcode, Program, ScanSource, Value, ValueSource,
};
use crate::vm::engine::JoinProgram;
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;

/// A window function kind (#74 follow-up), local to this planner --
/// mirrors `ast::ExprKind::FunctionCall`'s `OVER (...)` tail once resolved
/// by name, the same shape [`crate::vm::batch::WindowFunc`] executes but
/// kept as a separate type (same variants) so the planner's own vocabulary
/// doesn't depend on the VM's execution-operand enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowFunc {
    /// `ROW_NUMBER()`: 1-based position within the partition.
    RowNumber,
    /// `RANK()`: rank with gaps after ties.
    Rank,
    /// `DENSE_RANK()`: rank without gaps after ties.
    DenseRank,
    /// `LAG(col[, offset])`: the value `offset` rows before the current one.
    Lag,
    /// `LEAD(col[, offset])`: the value `offset` rows after the current one.
    Lead,
    /// `FIRST_VALUE(col)`: the value in the first row of the frame.
    FirstValue,
    /// `LAST_VALUE(col)`: the value in the last row of the frame.
    LastValue,
    /// `SUM(col) OVER (...)`: running sum over the frame.
    Sum,
    /// `AVG(col) OVER (...)`: running average over the frame.
    Avg,
    /// `COUNT(col) OVER (...)`: running count over the frame.
    Count,
}

impl WindowFunc {
    /// Resolves a (case-insensitive) SQL function name to its window kind, if any.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "ROW_NUMBER" => Some(WindowFunc::RowNumber),
            "RANK" => Some(WindowFunc::Rank),
            "DENSE_RANK" => Some(WindowFunc::DenseRank),
            "LAG" => Some(WindowFunc::Lag),
            "LEAD" => Some(WindowFunc::Lead),
            "FIRST_VALUE" => Some(WindowFunc::FirstValue),
            "LAST_VALUE" => Some(WindowFunc::LastValue),
            "SUM" => Some(WindowFunc::Sum),
            "AVG" => Some(WindowFunc::Avg),
            "COUNT" => Some(WindowFunc::Count),
            _ => None,
        }
    }

    /// Whether this window function takes no argument (`ROW_NUMBER()`,
    /// `RANK()`, `DENSE_RANK()`).
    pub fn is_niladic(self) -> bool {
        matches!(
            self,
            WindowFunc::RowNumber | WindowFunc::Rank | WindowFunc::DenseRank
        )
    }
}

/// `func(...) OVER (PARTITION BY ... ORDER BY ...)`, built directly from
/// an `ast::ExprKind::FunctionCall`'s `over: Some(WindowDef)` tail. `offset`
/// is only used by `LAG`/`LEAD` (default 1 when omitted).
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSpec {
    /// Which window function to evaluate.
    pub func: WindowFunc,
    /// The argument column, if the function takes one.
    pub arg: Option<String>,
    /// Row offset for `LAG`/`LEAD`; `None` means the default of 1.
    pub offset: Option<i64>,
    /// `PARTITION BY` column names.
    pub partition_by: Vec<String>,
    /// `ORDER BY` terms as `(column, descending)` pairs.
    pub order_by: Vec<(String, bool)>,
    /// `FILTER (WHERE ...)` predicate (#67): only `Sum`/`Avg`/`Count`
    /// accept one, per the SQL standard restricting `FILTER` to aggregate
    /// functions, not pure ranking functions.
    pub filter: Option<AstExpr>,
}

/// Planning failures: a column that resolves to no table, or a query shape
/// the executor doesn't implement. Storage-level failures (unknown table,
/// unreadable file) are the caller's, not the planner's.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanError {
    /// A referenced column that resolves to no table.
    UnknownColumn(String),
    /// A `WHERE col IN (SELECT ...)` shape [`compile_semi_join`] cannot plan.
    UnsupportedSemiJoin(String),
    /// `Right`/`Full`/`Cross` are parseable but only `Inner`/`Left` hash-
    /// join execution exists so far.
    UnsupportedJoinKind(JoinOp),
    /// `SELECT *` (or a mixed `SELECT col, *`) combined with `GROUP BY`, an
    /// aggregate, or a window function -- standard SQL rejects this too,
    /// since there's no well-defined column list to expand `*` into once
    /// the row shape is collapsed/reordered by those clauses.
    StarWithAggregation,
    /// A `SELECT`-list item this planner doesn't recognize -- reached only
    /// when `select` didn't pass `parser::column`'s validator first (that
    /// validator rejects every one of these with a `Span`-carrying error
    /// before this module ever sees the query).
    UnsupportedSelectItem(String),
    /// [`compile_join`] was handed a `SELECT` with no `JOIN` clause --
    /// dispatch routes only joined queries here, so this is a caller bug,
    /// surfaced as an error rather than a panic (db-core#231).
    NoJoinClause,
    /// db-core#232: a planner invariant did not hold (a `FROM` subquery
    /// reached the planner without the alias the grammar requires, ...).
    /// A codegen bug, never a property of the SQL; before, these sites
    /// fell back to an empty name and planned a wrong program.
    Internal(String),
    /// A `SINCE`/`UNTIL` clause (ADR 0018) reached the batch planner —
    /// that clause is stream-only; `codegen::stream::compile` strips it
    /// before delegating the residual query here.
    ScopeClauseUnsupported,
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::UnknownColumn(name) => write!(f, "unknown column: {name}"),
            PlanError::UnsupportedSemiJoin(msg) => write!(f, "unsupported semi-join: {msg}"),
            PlanError::UnsupportedJoinKind(kind) => write!(
                f,
                "join kind {kind:?} is not yet executable (only Inner/Left are implemented)"
            ),
            PlanError::StarWithAggregation => write!(
                f,
                "SELECT * cannot be combined with GROUP BY, an aggregate, or a window function"
            ),
            PlanError::UnsupportedSelectItem(msg) => write!(f, "unsupported SELECT item: {msg}"),
            PlanError::NoJoinClause => write!(f, "compile_join requires a JOIN clause"),
            PlanError::Internal(reason) => write!(f, "planner invariant violated: {reason}"),
            PlanError::ScopeClauseUnsupported => {
                write!(f, "SINCE/UNTIL is only available through the stream engine")
            }
        }
    }
}

impl std::error::Error for PlanError {}

/// Result alias for planner operations, with [`PlanError`] as the error type.
pub type Result<T> = std::result::Result<T, PlanError>;

// ---------------------------------------------------------------------
// ast::Select helpers -- classify/extract what the planner needs without
// building an intermediate AST type.
// ---------------------------------------------------------------------

/// One classified `SELECT`-list item.
#[derive(Debug, Clone, PartialEq)]
enum Item {
    Column(String),
    Star,
    /// `agg([DISTINCT] arg) [FILTER (WHERE ...)]`; the third field is the
    /// filter predicate, if any, the fourth whether it is `DISTINCT` (only
    /// ever `COUNT(DISTINCT x)`, #633).
    Agg(AggFunc, Option<AggArg>, Option<AstExpr>, bool),
    Window(WindowSpec),
    Expr(AstExpr),
    /// A scalar expression over one or more aggregate calls (#496: `SUM(x)
    /// * 2`, `SUM(x) + SUM(y)`, `SUM(x) / COUNT(*)`) -- anything
    /// `expr_contains_agg` catches that isn't itself a bare aggregate call.
    AggExpr(AstExpr),
}

/// True if `expr` contains an aggregate call anywhere in its tree (#496).
/// Mirrors `parser::column::expr_contains_agg`, which classifies the same
/// shape for its "columns outside an aggregate must be `GROUP BY` keys"
/// check -- kept as its own small copy here rather than shared across the
/// parser/planner boundary, like `is_known_agg_name` and `AggFunc` already
/// are for "is this name an aggregate". It walks the same nodes as
/// [`agg_expr_calls`]: an aggregate it misses is compiled as a per-row
/// expression and never merged.
fn expr_contains_agg(expr: &AstExpr) -> bool {
    match &expr.kind {
        ExprKind::FunctionCall { name, .. } if AggFunc::from_name(name).is_some() => true,
        ExprKind::FunctionCall {
            args: FunctionArgs::List(list),
            ..
        } => list.iter().any(expr_contains_agg),
        ExprKind::Binary { lhs, rhs, .. }
        | ExprKind::Is { lhs, rhs, .. }
        | ExprKind::Like {
            expr: lhs,
            pattern: rhs,
            ..
        } => expr_contains_agg(lhs) || expr_contains_agg(rhs),
        ExprKind::Unary { expr: inner, .. }
        | ExprKind::IsNull { expr: inner, .. }
        | ExprKind::Paren(inner) => expr_contains_agg(inner),
        _ => false,
    }
}

/// An aggregate's single argument (`None` is `COUNT(*)`): a bare column,
/// the common case (still tracked by name rather than a one-node `Expr`,
/// so `ctx.load_column` keeps memoizing it against the same register a
/// plain projection of that column would use), or an arbitrary expression
/// (#496, e.g. `SUM(amount * 2)`) computed once via `compile_expr` into a
/// fresh register before the aggregate reads it.
#[derive(Debug, Clone, PartialEq)]
enum AggArg {
    Column(String),
    Expr(AstExpr),
}

/// A (possibly qualified) column reference: `col` or `table.col`. Table
/// aliases are resolved by `parser::column` before this module ever sees
/// the query, so a qualifier here is always a real table name.
fn expr_column_name(expr: &AstExpr) -> Option<String> {
    match &expr.kind {
        ExprKind::Column {
            table: None,
            catalog: None,
            name,
        } => Some(name.clone()),
        ExprKind::Column {
            table: Some(table),
            catalog: None,
            name,
        } => Some(format!("{table}.{name}")),
        _ => None,
    }
}

/// The `#307` scalar functions this planner accepts in a `SELECT` list
/// (checked by name + arity, not the full `crate::functions::call`
/// registry -- everything else in that registry has never been callable
/// from SQL here, and opening that up is a separate, unscoped decision).
fn is_known_scalar_function(name: &str, arity: usize) -> bool {
    matches!(
        (name.to_ascii_lowercase().as_str(), arity),
        ("json_extract", 2) | ("logfmt_extract", 2) | ("regexp_extract", 3) | ("ilike", 2)
    ) || crate::vm::map_fns::MAP_FUNCTIONS
        .iter()
        .any(|(f, n)| f.eq_ignore_ascii_case(name) && *n == arity)
}

fn arg_count(args: &FunctionArgs) -> usize {
    match args {
        FunctionArgs::Star => 0,
        FunctionArgs::List(list) => list.len(),
    }
}

fn agg_arg(_expr: &AstExpr, agg: AggFunc, args: &FunctionArgs) -> Result<Option<AggArg>> {
    match args {
        FunctionArgs::Star => {
            if agg != AggFunc::Count {
                return Err(PlanError::UnsupportedSelectItem(
                    "only COUNT supports (*)".into(),
                ));
            }
            Ok(None)
        }
        // `parser::column::validate_aggregate_arg` (#496) already rejects a
        // nested aggregate and anything outside this subset's expression
        // grammar, so anything that isn't a bare column reference here is
        // an arbitrary expression this planner can `compile_expr` on its
        // own terms.
        FunctionArgs::List(list) => match list.as_slice() {
            [one] => Ok(Some(match expr_column_name(one) {
                Some(name) => AggArg::Column(name),
                None => AggArg::Expr(one.clone()),
            })),
            _ => Err(PlanError::UnsupportedSelectItem(
                "an aggregate takes exactly one column or *".into(),
            )),
        },
    }
}

/// Every aggregate call reachable from an `Item::AggExpr`'s expression
/// (#496), in left-to-right order -- the fixed traversal order the
/// pre-`Filter` source-loading pass and the register-allocation pass in
/// `compile` must agree on, so the same call resolves to the same source
/// register in both. It walks every node `parser::column::validate_expr`
/// lets an aggregate sit under (a scalar function's arguments are not
/// among them).
fn agg_expr_calls(expr: &AstExpr) -> Vec<&AstExpr> {
    match &expr.kind {
        ExprKind::FunctionCall { name, .. } if AggFunc::from_name(name).is_some() => vec![expr],
        ExprKind::Binary { lhs, rhs, .. }
        | ExprKind::Is { lhs, rhs, .. }
        | ExprKind::Like {
            expr: lhs,
            pattern: rhs,
            ..
        } => {
            let mut out = agg_expr_calls(lhs);
            out.extend(agg_expr_calls(rhs));
            out
        }
        ExprKind::Paren(inner)
        | ExprKind::Unary { expr: inner, .. }
        | ExprKind::IsNull { expr: inner, .. } => agg_expr_calls(inner),
        _ => vec![],
    }
}

/// The source register of one aggregate call nested in an expression
/// (#496) or in `HAVING` (#644), resolved before `WHERE`'s `Filter` like
/// every other aggregate source, `FILTER (WHERE ...)` folded in.
fn agg_call_source(call: &AstExpr, ctx: &mut Ctx) -> Result<Option<usize>> {
    let ExprKind::FunctionCall {
        name,
        distinct,
        args,
        tail,
    } = &call.kind
    else {
        return Err(PlanError::UnsupportedSelectItem(
            "internal: agg_expr_calls returned a non-FunctionCall node".into(),
        ));
    };
    if *distinct {
        return Err(PlanError::UnsupportedSelectItem(format!(
            "{name}(DISTINCT ...) inside an expression"
        )));
    }
    let func = AggFunc::from_name(name).unwrap_or(AggFunc::Count);
    // Its partial is a sketch no expression or `HAVING` operand can read (#603).
    if func == AggFunc::ApproxCountDistinct {
        return Err(PlanError::UnsupportedSelectItem(format!(
            "{name} inside an expression or HAVING (only as a SELECT-list item)"
        )));
    }
    let base = agg_arg(call, func, args)?.map(|a| match a {
        AggArg::Column(name) => ctx.load_column(&name),
        AggArg::Expr(e) => compile_expr(&e, ctx),
    });
    let filter = tail.as_deref().and_then(|t| t.filter.as_ref());
    Ok(mask_filtered_source(ctx, base, filter))
}

/// `HAVING` (#644) with each bare column that is not a `GROUP BY` key but
/// names a select-list alias replaced by that item's expression, as
/// sqlite3 and DuckDB resolve `HAVING n > 1` for `count(*) AS n`.
fn having_with_aliases(having: &AstExpr, select: &Select) -> AstExpr {
    let mut out = having.clone();
    substitute_aliases(&mut out, select);
    out
}

fn substitute_aliases(expr: &mut AstExpr, select: &Select) {
    if let ExprKind::Column {
        table: None, name, ..
    } = &expr.kind
    {
        let is_key = select
            .group_by
            .iter()
            .any(|t| expr_column_name(t).as_deref() == Some(name.as_str()));
        let aliased = select.columns.iter().find_map(|col| match col {
            ResultColumn::Expr {
                expr,
                alias: Some(alias),
            } if alias == name => Some(expr),
            _ => None,
        });
        if let (false, Some(aliased)) = (is_key, aliased) {
            *expr = aliased.clone();
        }
        return;
    }
    match &mut expr.kind {
        ExprKind::Binary { lhs, rhs, .. }
        | ExprKind::Is { lhs, rhs, .. }
        | ExprKind::Like {
            expr: lhs,
            pattern: rhs,
            ..
        } => {
            substitute_aliases(lhs, select);
            substitute_aliases(rhs, select);
        }
        ExprKind::Unary { expr: inner, .. }
        | ExprKind::IsNull { expr: inner, .. }
        | ExprKind::Paren(inner) => substitute_aliases(inner, select),
        ExprKind::FunctionCall {
            args: FunctionArgs::List(list),
            ..
        } => list.iter_mut().for_each(|a| substitute_aliases(a, select)),
        _ => {}
    }
}

/// Whether two aggregate calls are the same aggregate: the same function
/// (any case), `DISTINCT` or not, and the same arguments as SQL text.
fn same_aggregate(a: &AstExpr, b: &AstExpr) -> bool {
    match (&a.kind, &b.kind) {
        (
            ExprKind::FunctionCall {
                name: na,
                distinct: da,
                args: aa,
                ..
            },
            ExprKind::FunctionCall {
                name: nb,
                distinct: db,
                args: ab,
                ..
            },
        ) => {
            na.eq_ignore_ascii_case(nb)
                && da == db
                && match (aa, ab) {
                    (FunctionArgs::Star, FunctionArgs::Star) => true,
                    (FunctionArgs::List(la), FunctionArgs::List(lb)) => {
                        la.len() == lb.len()
                            && la
                                .iter()
                                .zip(lb)
                                .all(|(x, y)| x.to_string() == y.to_string())
                    }
                    _ => false,
                }
        }
        _ => false,
    }
}

/// Where each `HAVING` aggregate call (#644) finds its value: the select
/// list's own merged column for the same aggregate ([`same_aggregate`])
/// -- item `i`'s `k`th call -- or `None`, a column of its own.
/// `COUNT(DISTINCT x)` must be the select list's: a query has one
/// distinct key.
fn having_reuse(
    calls: &[&AstExpr],
    select: &Select,
    items: &[Item],
) -> Result<Vec<Option<(usize, usize)>>> {
    calls
        .iter()
        .map(|call| {
            let found =
                select
                    .columns
                    .iter()
                    .zip(items)
                    .enumerate()
                    .find_map(|(i, (col, item))| {
                        let ResultColumn::Expr { expr, .. } = col else {
                            return None;
                        };
                        match item {
                            Item::Agg(..) => same_aggregate(expr, call).then_some((i, 0)),
                            Item::AggExpr(_) => agg_expr_calls(expr)
                                .iter()
                                .position(|c| same_aggregate(c, call))
                                .map(|k| (i, k)),
                            _ => None,
                        }
                    });
            let distinct = matches!(&call.kind, ExprKind::FunctionCall { distinct: true, .. });
            if distinct && found.is_none() {
                return Err(PlanError::UnsupportedSelectItem(
                    "COUNT(DISTINCT ...) in HAVING must also be in the SELECT list".into(),
                ));
            }
            Ok(found)
        })
        .collect()
}

/// Adds one aggregate over `src` to `compile`'s accumulators as a merged
/// output column and returns that column's position: `AVG` is two
/// partials (sum, count) that merge into one column, every other
/// aggregate is one.
fn push_aggregate(
    func: AggFunc,
    src: Option<usize>,
    aggs: &mut Vec<(AggFunc, Option<usize>)>,
    agg_dst: &mut Vec<usize>,
    agg_parts: &mut Vec<AggPart>,
    emit_regs: &mut Vec<usize>,
    ctx: &mut Ctx,
) -> usize {
    let column = agg_parts.len();
    // Resolved up front so the dispatch below has no "can't happen"
    // arm: `None` *is* the `Avg` case (the only aggregate that needs
    // two partials), not a wildcard hiding one.
    let simple_part = match func {
        AggFunc::Sum => Some(AggPart::Sum),
        AggFunc::Count => Some(AggPart::Count),
        AggFunc::Min => Some(AggPart::Min),
        AggFunc::Max => Some(AggPart::Max),
        AggFunc::ApproxCountDistinct => Some(AggPart::ApproxCountDistinct),
        // `AvgSum` never comes from SQL; it is `Avg`'s partial.
        AggFunc::Avg | AggFunc::AvgSum => None,
    };
    match simple_part {
        Some(part) => {
            let dst = ctx.alloc();
            aggs.push((func, src));
            agg_dst.push(dst);
            agg_parts.push(part);
            emit_regs.push(dst);
        }
        None => {
            let sum_dst = ctx.alloc();
            let count_dst = ctx.alloc();
            aggs.push((AggFunc::AvgSum, src));
            agg_dst.push(sum_dst);
            aggs.push((AggFunc::Count, src));
            agg_dst.push(count_dst);
            let sum_pos = emit_regs.len();
            agg_parts.push(AggPart::Avg(sum_pos, sum_pos.saturating_add(1)));
            emit_regs.push(sum_dst);
            emit_regs.push(count_dst);
        }
    }
    column
}

/// A `GROUP BY` term's text for plan comments.
fn group_term_label(term: &AstExpr) -> String {
    match expr_to_string(term) {
        label if label.is_empty() => "<expression>".to_string(),
        label => label,
    }
}

/// Whether `item_expr` is the `GROUP BY` key `term`: the same column by
/// name, or the very expression [`expand_star`] copied into `GROUP BY`
/// from a `SELECT`-list alias (#307), so equal including its spans.
fn is_group_term(item_expr: &AstExpr, term: &AstExpr) -> bool {
    match (expr_column_name(item_expr), expr_column_name(term)) {
        (Some(a), Some(b)) => a == b,
        _ => item_expr == term,
    }
}

/// Whether a grouped or aggregate `SELECT` needs `Combine`'s post-
/// aggregation projection (#623): yes for any expression over aggregates,
/// and for a `GROUP BY` select list that is not exactly its keys, in
/// order, followed by plain aggregates (a key computed on, reordered,
/// repeated or left out).
fn needs_projection(select: &Select, items: &[Item]) -> bool {
    if items.iter().any(|i| matches!(i, Item::AggExpr(_))) {
        return true;
    }
    if select.group_by.is_empty() {
        return false;
    }
    let mut keys = Vec::new();
    for (col, item) in select.columns.iter().zip(items) {
        if let (Item::Column(_) | Item::Expr(_), ResultColumn::Expr { expr, .. }) = (item, col) {
            keys.push(expr);
        }
    }
    keys.len() != select.group_by.len()
        || keys
            .iter()
            .zip(&select.group_by)
            .any(|(expr, term)| !is_group_term(expr, term))
}

/// Builds `Combine`'s projection (#623): each select item rewritten onto
/// the merged columns (`#i` is merged column `i`: the keys first, then
/// the aggregates at the positions `item_slots` recorded) and compiled
/// like any other expression, then one `Emit` in select order. A
/// `HAVING` (#644) is a `Filter` over the merged columns before them.
fn build_projection(
    select: &Select,
    items: &[Item],
    item_slots: &[Vec<usize>],
    having: Option<(&AstExpr, &[&AstExpr], &[usize])>,
) -> Result<Vec<Opcode>> {
    let mut ctx = Ctx {
        next_reg: 0,
        column_regs: HashMap::new(),
        program: Vec::new(),
    };
    // #644: `HAVING` filters the merged rows first; its aggregate calls
    // are merged columns like the select list's.
    if let Some((expr, calls, slots)) = having {
        let rewritten = onto_merged(expr, &select.group_by, calls, slots)?;
        let predicate = compile_expr(&rewritten, &mut ctx);
        ctx.push(Opcode::Filter { predicate });
    }
    let mut registers = Vec::with_capacity(items.len());
    for ((col, item), slots) in select.columns.iter().zip(items).zip(item_slots) {
        let ResultColumn::Expr { expr, .. } = col else {
            return Err(PlanError::StarWithAggregation);
        };
        let rewritten = match item {
            Item::Agg(..) => {
                let slot = slots.first().copied().ok_or_else(|| {
                    PlanError::Internal("an aggregate item without a merged column".into())
                })?;
                merged_column(slot)
            }
            Item::Column(_) | Item::Expr(_) | Item::AggExpr(_) => {
                let calls = agg_expr_calls(expr);
                onto_merged(expr, &select.group_by, &calls, slots)?
            }
            Item::Star | Item::Window(_) => return Err(PlanError::StarWithAggregation),
        };
        registers.push(compile_expr(&rewritten, &mut ctx));
    }
    ctx.push(Opcode::Emit {
        registers: registers.into(),
    });
    Ok(ctx.program.into_iter().map(|i| i.opcode).collect())
}

/// A reference to merged column `slot` in the projection's batch.
fn merged_column(slot: usize) -> AstExpr {
    AstExpr {
        kind: ExprKind::Column {
            table: None,
            catalog: None,
            name: format!("#{slot}"),
        },
        span: crate::parser::Span::UNKNOWN,
    }
}

/// `expr` with every `GROUP BY` key (`terms[i]` is merged column `i`) and
/// every aggregate call (`calls[k]` is merged column `slots[k]`) replaced
/// by a merged-column reference. Any other node is kept, except a column:
/// one left over is neither a key nor aggregated, so it is refused here,
/// before it could be mistaken for a merged column by name (a real column
/// may be called `#0`).
fn onto_merged(
    expr: &AstExpr,
    terms: &[AstExpr],
    calls: &[&AstExpr],
    slots: &[usize],
) -> Result<AstExpr> {
    if let Some(i) = terms.iter().position(|t| is_group_term(expr, t)) {
        return Ok(merged_column(i));
    }
    if let ExprKind::FunctionCall { name, .. } = &expr.kind {
        if AggFunc::from_name(name).is_some() {
            let slot = calls
                .iter()
                .position(|c| *c == expr)
                .and_then(|k| slots.get(k))
                .ok_or_else(|| {
                    PlanError::Internal(format!("aggregate {name} has no merged column"))
                })?;
            return Ok(merged_column(*slot));
        }
    }
    let recurse = |e: &AstExpr| onto_merged(e, terms, calls, slots).map(Box::new);
    let mut out = expr.clone();
    match &mut out.kind {
        ExprKind::Binary { lhs, rhs, .. } | ExprKind::Is { lhs, rhs, .. } => {
            *lhs = recurse(lhs)?;
            *rhs = recurse(rhs)?;
        }
        ExprKind::Like {
            expr: inner,
            pattern,
            ..
        } => {
            *inner = recurse(inner)?;
            *pattern = recurse(pattern)?;
        }
        ExprKind::Paren(inner)
        | ExprKind::Unary { expr: inner, .. }
        | ExprKind::IsNull { expr: inner, .. } => *inner = recurse(inner)?,
        ExprKind::FunctionCall {
            args: FunctionArgs::List(list),
            ..
        } => {
            for arg in list.iter_mut() {
                *arg = onto_merged(arg, terms, calls, slots)?;
            }
        }
        ExprKind::Column { .. } => {
            let name = expr_column_name(expr).unwrap_or_default();
            return Err(PlanError::UnsupportedSelectItem(format!(
                "column `{name}` must be a GROUP BY key or inside an aggregate"
            )));
        }
        ExprKind::InSubquery { .. } | ExprKind::Exists { .. } => {
            return Err(PlanError::UnsupportedSelectItem(
                "a subquery in a grouped or aggregate SELECT list".into(),
            ));
        }
        _ => {}
    }
    Ok(out)
}

/// Lowers `name(args) OVER (window_def)` into a [`WindowSpec`]: resolves
/// `name` against [`WindowFunc::from_name`] and converts `window_def`'s
/// `PARTITION BY`/`ORDER BY` expressions to plain column names.
fn window_spec(
    name: &str,
    args: &FunctionArgs,
    window_def: &crate::parser::ast::WindowDef,
) -> Result<WindowSpec> {
    let func = WindowFunc::from_name(name).ok_or_else(|| {
        PlanError::UnsupportedSelectItem(format!("unknown window function {name}"))
    })?;

    let (arg, offset) = match (func.is_niladic(), args) {
        (true, FunctionArgs::List(list)) if list.is_empty() => (None, None),
        (true, _) => {
            return Err(PlanError::UnsupportedSelectItem(format!(
                "{name} takes no arguments"
            )))
        }
        (false, FunctionArgs::Star) => {
            if func != WindowFunc::Count {
                return Err(PlanError::UnsupportedSelectItem(
                    "only COUNT supports (*)".into(),
                ));
            }
            (None, None)
        }
        (false, FunctionArgs::List(list)) if matches!(func, WindowFunc::Lag | WindowFunc::Lead) => {
            match list.as_slice() {
                [one] => (expr_column_name(one), None),
                [one, offset_expr] => {
                    let offset = match &offset_expr.kind {
                        ExprKind::Literal(AstLiteral::Integer(n)) => Some(*n),
                        _ => {
                            return Err(PlanError::UnsupportedSelectItem(
                                "non-integer offset".into(),
                            ))
                        }
                    };
                    (expr_column_name(one), offset)
                }
                _ => {
                    return Err(PlanError::UnsupportedSelectItem(format!(
                        "{name} takes 1 or 2 arguments"
                    )))
                }
            }
        }
        (false, FunctionArgs::List(list)) => match list.as_slice() {
            [one] => (expr_column_name(one), None),
            _ => {
                return Err(PlanError::UnsupportedSelectItem(
                    "a window function takes exactly one column or *".into(),
                ))
            }
        },
    };

    let partition_by = window_def
        .partition_by
        .iter()
        .map(|e| {
            expr_column_name(e).ok_or_else(|| {
                PlanError::UnsupportedSelectItem("expected a column reference".into())
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let order_by = window_def
        .order_by
        .iter()
        .map(|term| {
            expr_column_name(&term.expr)
                .map(|c| (c, term.desc.unwrap_or(false)))
                .ok_or_else(|| {
                    PlanError::UnsupportedSelectItem("expected a column reference".into())
                })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(WindowSpec {
        func,
        arg,
        offset,
        partition_by,
        order_by,
        filter: None,
    })
}

/// Whether any `SELECT`-list column carries a `RANGE <duration>` tail
/// (#308, stream-only, like [`Select::scope`]) -- `codegen::stream`
/// always strips this into a residual `Select` before delegating to
/// [`compile`], so seeing one here means a row/batch caller is compiling
/// a range-vector query directly, which this planner does not implement.
fn has_range_vector_call(select: &Select) -> bool {
    select.columns.iter().any(|col| {
        matches!(
            col,
            ResultColumn::Expr {
                expr: AstExpr {
                    kind: ExprKind::FunctionCall { tail, .. },
                    ..
                },
                ..
            } if tail.as_deref().is_some_and(|t| t.range.is_some())
        )
    })
}

fn classify_item(col: &ResultColumn) -> Result<Item> {
    match col {
        ResultColumn::Star => Ok(Item::Star),
        ResultColumn::TableStar { .. } => Err(PlanError::UnsupportedSelectItem(
            "table.* is not supported".into(),
        )),
        // `alias` only renames this column's output header
        // ([`output_column_names`]) and, for a `GROUP BY` that names it
        // exactly, its group key ([`resolve_group_by`]) -- it does
        // not change how `expr` itself classifies or compiles.
        ResultColumn::Expr { expr, alias: _ } => match &expr.kind {
            ExprKind::Column { .. } => expr_column_name(expr).map(Item::Column).ok_or_else(|| {
                PlanError::UnsupportedSelectItem("expected a column reference".into())
            }),
            ExprKind::FunctionCall {
                name,
                distinct,
                args,
                tail,
            } if matches!(tail.as_deref(), Some(t) if t.over.is_some()) => {
                if *distinct {
                    return Err(PlanError::UnsupportedSelectItem(format!(
                        "DISTINCT in the window function {name}"
                    )));
                }
                // The guard above makes the `else` unreachable; a typed
                // error keeps it total without an `expect` (db-core#231).
                let Some(window_def) = tail.as_deref().and_then(|t| t.over.as_ref()) else {
                    return Err(PlanError::UnsupportedSelectItem(
                        "window function call without an OVER clause".into(),
                    ));
                };
                let mut spec = window_spec(name, args, window_def)?;
                let filter = tail.as_deref().and_then(|t| t.filter.as_ref());
                if let Some(f) = filter {
                    if !matches!(
                        spec.func,
                        WindowFunc::Sum | WindowFunc::Avg | WindowFunc::Count
                    ) {
                        return Err(PlanError::UnsupportedSelectItem(format!(
                            "FILTER is not supported on {name} (only SUM/AVG/COUNT window aggregates accept it)"
                        )));
                    }
                    spec.filter = Some(f.clone());
                }
                Ok(Item::Window(spec))
            }
            // A range-vector call (#308): `has_range_vector_call` rejects
            // this at `compile`'s own entry, but `classify_item` is also
            // called from `expand_star` (only to detect `SELECT *`,
            // regardless of what any non-`*` item turns out to be), so
            // this arm must not error here -- `Item::Expr` is never
            // actually compiled for this shape.
            ExprKind::FunctionCall { tail, .. }
                if tail.as_deref().is_some_and(|t| t.range.is_some()) =>
            {
                Ok(Item::Expr(expr.clone()))
            }
            ExprKind::FunctionCall {
                name,
                distinct,
                args,
                tail,
            } => match AggFunc::from_name(name) {
                Some(agg) => {
                    let arg = agg_arg(expr, agg, args)?;
                    if *distinct && (agg != AggFunc::Count || arg.is_none()) {
                        return Err(PlanError::UnsupportedSelectItem(format!(
                            "{name}(DISTINCT ...): only COUNT(DISTINCT x) is supported"
                        )));
                    }
                    let filter = tail.as_deref().and_then(|t| t.filter.clone());
                    Ok(Item::Agg(agg, arg, filter, *distinct))
                }
                // Not an aggregate: a scalar call (#307) compiles like any
                // other expression (`compile_expr`'s `FunctionCall` arm);
                // anything else is rejected here, at plan time (ADR 0002),
                // rather than silently lowering to `NULL` in `compile_expr`.
                None if is_known_scalar_function(name, arg_count(args)) => {
                    Ok(Item::Expr(expr.clone()))
                }
                None => Err(PlanError::UnsupportedSelectItem(format!(
                    "unknown function {name}"
                ))),
            },
            _ if expr_contains_agg(expr) => Ok(Item::AggExpr(expr.clone())),
            _ => Ok(Item::Expr(expr.clone())),
        },
    }
}

/// Every `SELECT`-list `AS <alias>` in `select`, mapped to its
/// underlying expression -- #307's narrow alias support: only a `GROUP
/// BY` that names an alias, and no real column (#623), resolves through
/// this map ([`expand_star`]); nothing else in this planner looks a name
/// up against a `SELECT`-list alias.
fn select_aliases(select: &Select) -> HashMap<String, AstExpr> {
    select
        .columns
        .iter()
        .filter_map(|col| match col {
            ResultColumn::Expr {
                expr,
                alias: Some(alias),
            } => Some((alias.clone(), expr.clone())),
            _ => None,
        })
        .collect()
}

fn classify_items(select: &Select) -> Result<Vec<Item>> {
    select.columns.iter().map(classify_item).collect()
}

/// The table this `FROM` clause is scanned/joined against: a real table's
/// name, or a `FROM`-subquery's mandatory alias.
fn table_name(from: &AstFromClause) -> Result<&str> {
    match &from.first.kind {
        TableRefKind::Name(name) => Ok(name),
        // The grammar requires the alias; its absence is a planner bug,
        // not a table called "" (db-core#232).
        TableRefKind::Subquery(_) => from.first.alias.as_deref().ok_or_else(|| {
            PlanError::Internal("FROM subquery reached the planner without an alias".into())
        }),
    }
}

/// Extract `(left_col, right_col)` from a `JOIN ... ON <expr>` condition;
/// `Cross` with no constraint is the only join kind with no condition.
fn extract_equi_join(expr: &AstExpr) -> Option<(String, String)> {
    match &expr.kind {
        ExprKind::Binary {
            op: AstBinOp::Eq,
            lhs,
            rhs,
        } => Some((expr_column_name(lhs)?, expr_column_name(rhs)?)),
        _ => None,
    }
}

/// One equi-join step, extracted from an `ast::Join`.
struct JoinStep {
    op: JoinOp,
    table: String,
    left_col: String,
    right_col: String,
    /// The `(FROM column, joined column)` pairs of a `USING (a, b, ...)`
    /// past the first, which keys the hash; they filter the joined rows.
    extra_keys: Vec<(String, String)>,
}

/// `select` with every join's extra `USING` keys added to its `WHERE` as
/// `from.k = joined.k` (#771): the hash join keys on the first column and
/// these keep the rows that agree on the rest. Only an inner join can drop
/// a non-matching row, so an outer join with extra keys is refused.
fn with_extra_key_filters(select: &Select, from_name: &str, joins: &[JoinStep]) -> Result<Select> {
    let mut filtered = select.clone();
    for join in joins {
        if join.extra_keys.is_empty() {
            continue;
        }
        if join.op != JoinOp::Inner {
            return Err(PlanError::UnsupportedJoinKind(join.op));
        }
        for (from_col, join_col) in &join.extra_keys {
            let equal = AstExpr {
                kind: ExprKind::Binary {
                    op: AstBinOp::Eq,
                    lhs: Box::new(key_column(from_name, from_col)),
                    rhs: Box::new(key_column(&join.table, join_col)),
                },
                span: crate::parser::Span::UNKNOWN,
            };
            filtered.where_clause = Some(match filtered.where_clause.take() {
                Some(existing) => AstExpr {
                    kind: ExprKind::Binary {
                        op: AstBinOp::And,
                        lhs: Box::new(existing),
                        rhs: Box::new(equal),
                    },
                    span: crate::parser::Span::UNKNOWN,
                },
                None => equal,
            });
        }
    }
    Ok(filtered)
}

fn key_column(table: &str, name: &str) -> AstExpr {
    AstExpr {
        kind: ExprKind::Column {
            table: Some(table.to_string()),
            catalog: None,
            name: name.to_string(),
        },
        span: crate::parser::Span::UNKNOWN,
    }
}

fn extract_joins(from: &AstFromClause) -> Result<Vec<JoinStep>> {
    let from_name = table_name(from)?;
    from.joins
        .iter()
        .map(|j| {
            let TableRefKind::Name(table) = &j.table.kind else {
                return Err(PlanError::UnknownColumn(
                    "subquery in JOIN is not supported by the batch planner".into(),
                ));
            };
            let mut extra_keys = Vec::new();
            let (left_col, right_col) = match &j.constraint {
                Some(JoinConstraint::On(expr)) => {
                    let (left, right) = extract_equi_join(expr).ok_or_else(|| {
                        PlanError::UnknownColumn("JOIN ON must be col = col".into())
                    })?;
                    // `ON j.k = f.k` names the joined table first: the
                    // pair is `(FROM column, joined column)` either way.
                    let owners = (split_qualified(&left).0, split_qualified(&right).0);
                    if owners == (Some(table.as_str()), Some(from_name)) {
                        (right, left)
                    } else {
                        (left, right)
                    }
                }
                // `USING (k)` is `ON <from>.k = <joined>.k` (#771): the
                // `FROM` table's value is the one an unqualified `k`
                // names, as in SQLite and DuckDB. Further columns of
                // `USING (k, l)` are filters, see `with_extra_key_filters`.
                Some(JoinConstraint::Using(columns)) => match columns.split_first() {
                    Some((key, rest)) => {
                        extra_keys = rest.iter().map(|c| (c.clone(), c.clone())).collect();
                        (format!("{from_name}.{key}"), format!("{table}.{key}"))
                    }
                    None => {
                        return Err(PlanError::UnknownColumn(
                            "USING takes at least one key column".into(),
                        ))
                    }
                },
                None if j.op == JoinOp::Cross => (String::new(), String::new()),
                _ => {
                    return Err(PlanError::UnknownColumn(
                        "unsupported JOIN condition".into(),
                    ))
                }
            };
            Ok(JoinStep {
                op: j.op,
                table: table.clone(),
                left_col,
                right_col,
                extra_keys,
            })
        })
        .collect()
}

fn literal_value(lit: &AstLiteral) -> Value {
    match lit {
        AstLiteral::Integer(v) => Value::Int(*v),
        AstLiteral::Float(v) => Value::Float(*v),
        AstLiteral::Str(v) => Value::Str(v.clone().into()),
        // Blob/Null/True/False aren't part of the batch planner's literal
        // subset (`parser::column`'s validator rejects them before this
        // module ever runs) -- fall back to NULL rather than panicking.
        AstLiteral::Blob(_) | AstLiteral::Null | AstLiteral::True | AstLiteral::False => {
            Value::Null
        }
    }
}

/// `OFFSET m` (a non-negative integer literal, as the column validator
/// requires); 0 without one (#679).
fn select_offset(select: &Select) -> usize {
    match select
        .limit
        .as_ref()
        .and_then(|l| l.offset.as_ref())
        .map(|o| &o.kind)
    {
        Some(ExprKind::Literal(AstLiteral::Integer(m))) => usize::try_from(*m).unwrap_or(0),
        _ => 0,
    }
}

fn select_limit(select: &Select) -> Option<usize> {
    let limit = select.limit.as_ref()?;
    match &limit.limit.kind {
        // A negative LIMIT means "no limit" in SQLite (`LIMIT -1`).
        ExprKind::Literal(AstLiteral::Integer(n)) => usize::try_from(*n).ok(),
        _ => None,
    }
}

/// Lowers an aggregate `FunctionCall` (`COUNT(x)`, `COUNT(*)`, ...) to
/// the same output label [`select_item_label`] would render for it (via
/// [`AggFunc::name`]) -- used to resolve an `ORDER BY` reference to a
/// `SELECT`-list aggregate (#131) against that same label.
fn aggregate_call_label(name: &str, distinct: bool, args: &FunctionArgs) -> Option<String> {
    let agg = AggFunc::from_name(name)?;
    let distinct = if distinct { "DISTINCT " } else { "" };
    match args {
        FunctionArgs::Star if agg == AggFunc::Count => Some(format!("{}(*)", agg.name())),
        FunctionArgs::Star => None,
        FunctionArgs::List(list) => match list.as_slice() {
            [one] => expr_column_name(one).map(|col| format!("{}({distinct}{col})", agg.name())),
            _ => None,
        },
    }
}

/// Each `ORDER BY` term's resolved output-column reference (a bare column
/// name or a `SELECT`-list aggregate's rendered label), and whether it's
/// descending, most significant first (#645). A term with no such
/// reference is left out; `parser::column`'s validator admits none.
fn select_order_by(select: &Select) -> Vec<(String, bool)> {
    select
        .order_by
        .iter()
        .filter_map(|term| {
            let column = match &term.expr.kind {
                ExprKind::Column { .. } => expr_column_name(&term.expr)?,
                ExprKind::FunctionCall {
                    name,
                    distinct,
                    args,
                    tail,
                } if !matches!(tail.as_deref(), Some(t) if t.over.is_some()) => {
                    aggregate_call_label(name, *distinct, args)?
                }
                _ => return None,
            };
            Some((column, term.desc.unwrap_or(false)))
        })
        .collect()
}

/// [`Opcode::Sort`] for `select`'s `ORDER BY` (#645): each term's output
/// position, most significant first, with its plan comment; `None`
/// without one.
///
/// A term is a `SELECT`-list item -- a column by name or alias, an
/// aggregate by [`same_aggregate`] -- or, when `extra` is given (a query
/// that neither groups nor aggregates), a column outside the list: it is
/// loaded into `extra`'s context and emitted after the list, and the
/// `Sort` drops it again (`hidden`). Anything else is refused; such a
/// term used to drop the sort silently.
fn order_sort(
    select: &Select,
    mut extra: Option<(&mut Ctx, &mut Vec<usize>)>,
) -> Result<Option<(Opcode, String)>> {
    let terms = select_order_by(select);
    if terms.is_empty() {
        return Ok(None);
    }
    let mut keys = Vec::with_capacity(terms.len());
    let mut hidden = 0usize;
    for (term, (label, descending)) in select.order_by.iter().zip(&terms) {
        let pos = select
            .columns
            .iter()
            .position(|item| match (item, &term.expr.kind) {
                (ResultColumn::Expr { expr, .. }, ExprKind::FunctionCall { .. }) => {
                    same_aggregate(expr, &term.expr)
                }
                _ => select_item_label(item) == *label,
            });
        let pos = match (pos, &mut extra, &term.expr.kind) {
            (Some(pos), _, _) => pos,
            (None, Some((ctx, regs)), ExprKind::Column { .. }) => {
                if matches!(select.distinct, Some(Distinctness::Distinct)) {
                    return Err(PlanError::UnsupportedSelectItem(format!(
                        "ORDER BY {label}: with SELECT DISTINCT, an ORDER BY term must be in the SELECT list"
                    )));
                }
                let reg = ctx.load_column(label);
                regs.push(reg);
                hidden = hidden.saturating_add(1);
                regs.len().saturating_sub(1)
            }
            (None, ..) => {
                return Err(PlanError::UnsupportedSelectItem(format!(
                    "ORDER BY {label}: in a grouped or aggregate query, only a SELECT-list column or aggregate can be sorted on"
                )))
            }
        };
        keys.push((pos, *descending));
    }
    Ok(Some((
        Opcode::Sort {
            keys: keys.into(),
            hidden,
        },
        format!("ORDER BY {}", order_by_text(&terms)),
    )))
}

/// `a, b DESC`: the `ORDER BY` terms as plan text.
fn order_by_text(terms: &[(String, bool)]) -> String {
    terms
        .iter()
        .map(|(column, descending)| format!("{column}{}", if *descending { " DESC" } else { "" }))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Expand every `SELECT *` in `select.columns` into one plain column
/// reference per entry of `schema` (in `schema`'s order), leaving every
/// other select item untouched -- so `SELECT id, * FROM t` keeps `id`
/// first and expands `*` after it. `schema` is the resolved table's
/// column names; `sql-parser` never sees these (`ResultColumn::Star` is
/// left as an AST-level marker), so this is the caller's (the executor
/// with Parquet/table schema access) job to run once, before handing the
/// query to [`compile`]/[`compile_join`]/[`compile_semi_join`]/
/// [`compile_window`].
///
/// It also resolves each `GROUP BY` name against `schema` (#623): a real
/// column wins, as in SQLite and DuckDB; only a name that is no column
/// and is a `SELECT`-list alias (#307) becomes that alias's expression.
/// [`compile`] groups by the terms as given, so a query must come through
/// here first for an alias in `GROUP BY` to resolve.
///
/// Returns [`PlanError::StarWithAggregation`] if `*` is combined with
/// `GROUP BY` or an aggregate/window select item.
pub fn expand_star(select: &Select, schema: &[String]) -> Result<Select> {
    let select = &resolve_group_by(select, schema)?;
    let select = &lower_table_star(select)?;
    let items = classify_items(select)?;
    if !items.iter().any(|c| matches!(c, Item::Star)) {
        return Ok(select.clone());
    }
    let has_aggregation = !select.group_by.is_empty()
        || items
            .iter()
            .any(|c| matches!(c, Item::Agg(..) | Item::Window(_)));
    // `SELECT *` alongside an aggregate has no fixed column list to expand.
    if has_aggregation {
        return Err(PlanError::StarWithAggregation);
    }
    let mut columns = Vec::with_capacity(select.columns.len().saturating_add(schema.len()));
    for (col, item) in select.columns.iter().zip(&items) {
        match item {
            Item::Star => columns.extend(schema.iter().map(|name| ResultColumn::Expr {
                expr: crate::parser::ast::Expr {
                    kind: ExprKind::Column {
                        table: None,
                        catalog: None,
                        name: name.clone(),
                    },
                    span: crate::parser::Span::UNKNOWN,
                },
                alias: None,
            })),
            _ => columns.push(col.clone()),
        }
    }
    Ok(Select {
        columns,
        ..select.clone()
    })
}

/// `select` with `t.*` read as `*` when `t` is the `FROM` table (by name
/// or alias), which is what a single-table `t.*` means (#771). A `t.*`
/// naming another table is an unknown table here: `schema` is the `FROM`
/// table's, so a join's own `t.*` is expanded by its resolver.
fn lower_table_star(select: &Select) -> Result<Select> {
    let mut lowered = select.clone();
    let Some(from) = &select.from else {
        return Ok(lowered);
    };
    let first = (from.first.name(), from.first.alias.as_deref());
    for column in &mut lowered.columns {
        let ResultColumn::TableStar { table } = column else {
            continue;
        };
        let names = |n: Option<&str>| n.is_some_and(|n| n.eq_ignore_ascii_case(table));
        if ![first.0, first.1].into_iter().any(names) {
            return Err(PlanError::UnknownColumn(format!("unknown table: {table}")));
        }
        *column = ResultColumn::Star;
    }
    Ok(lowered)
}

/// `select` with each `GROUP BY` name that is not a column of `schema`
/// but is a `SELECT`-list alias replaced by the alias's expression
/// (#307/#623): a real column wins over an alias, as in DuckDB and
/// PostgreSQL. [`expand_star`] calls it with the table's columns; a join
/// caller (`engine::resolve`) calls it with every joined table's columns
/// before planning.
///
/// Returns [`PlanError::UnsupportedSelectItem`] for a name that resolves
/// to an aggregate's alias: `GROUP BY` cannot contain aggregates.
pub fn resolve_group_by(select: &Select, schema: &[String]) -> Result<Select> {
    let aliases = select_aliases(select);
    let mut resolved = select.clone();
    for term in &mut resolved.group_by {
        let Some(name) = expr_column_name(term) else {
            continue;
        };
        if schema.contains(&name) {
            continue;
        }
        if let Some(expr) = aliases.get(&name) {
            if expr_contains_agg(expr) {
                return Err(PlanError::UnsupportedSelectItem(format!(
                    "GROUP BY `{name}` names an aggregate; GROUP BY cannot contain aggregates"
                )));
            }
            *term = expr.clone();
        }
    }
    Ok(resolved)
}

/// `(column, items, negated)` when `expr` is an `OR` chain of at least two
/// `column = literal` (an `AND` chain of `column <> literal`, `negated`)
/// over one column: the shape `parser::column` lowers `column [NOT] IN
/// (literal, ...)` to, or the same written out by hand. Any other leaf, a
/// second column, or a single comparison is `None`, compiled as before.
fn in_list_chain(expr: &AstExpr) -> Option<(String, Vec<Value>, bool)> {
    let (join, cmp, negated) = match &expr.kind {
        ExprKind::Binary {
            op: AstBinOp::Or, ..
        } => (AstBinOp::Or, AstBinOp::Eq, false),
        ExprKind::Binary {
            op: AstBinOp::And, ..
        } => (AstBinOp::And, AstBinOp::Ne, true),
        _ => return None,
    };
    let leaves = chain_operands(expr, join);
    if leaves.len() < 2 {
        return None;
    }
    let mut column: Option<String> = None;
    let mut items = Vec::with_capacity(leaves.len());
    for leaf in leaves {
        let ExprKind::Binary { op, lhs, rhs } = &leaf.kind else {
            return None;
        };
        let (ExprKind::Literal(lit), Some(name)) = (&rhs.kind, expr_column_name(lhs)) else {
            return None;
        };
        if *op != cmp || column.as_ref().is_some_and(|c| *c != name) {
            return None;
        }
        column = Some(name);
        items.push(literal_value(lit));
    }
    column.map(|c| (c, items, negated))
}

/// The operands of `expr`'s nested `join` nodes (through parentheses), left
/// to right.
fn chain_operands(expr: &AstExpr, join: AstBinOp) -> Vec<&AstExpr> {
    match &expr.kind {
        ExprKind::Paren(inner) => chain_operands(inner, join),
        ExprKind::Binary { op, lhs, rhs } if *op == join => {
            let mut operands = chain_operands(lhs, join);
            operands.extend(chain_operands(rhs, join));
            operands
        }
        _ => vec![expr],
    }
}

fn map_bin_op(op: AstBinOp) -> MapOp {
    match op {
        AstBinOp::Add => MapOp::Add,
        AstBinOp::Sub => MapOp::Sub,
        AstBinOp::Mul => MapOp::Mul,
        AstBinOp::Div => MapOp::Div,
        AstBinOp::Eq => MapOp::Eq,
        AstBinOp::Ne => MapOp::Ne,
        AstBinOp::Lt => MapOp::Lt,
        AstBinOp::Le => MapOp::Le,
        AstBinOp::Gt => MapOp::Gt,
        AstBinOp::Ge => MapOp::Ge,
        AstBinOp::And => MapOp::And,
        AstBinOp::Or => MapOp::Or,
        AstBinOp::Concat => MapOp::Concat,
        // `parser::column`'s validator rejects every other operator
        // (bitwise/shift/modulo) before this module ever sees the query --
        // reaching here means an unvalidated `Select` was compiled
        // directly. Fall back to `Eq` rather than panicking.
        AstBinOp::BitAnd | AstBinOp::BitOr | AstBinOp::Shl | AstBinOp::Shr | AstBinOp::Mod => {
            MapOp::Eq
        }
    }
}

/// Register allocator + column memo shared by [`compile`]'s helpers.
struct Ctx {
    next_reg: usize,
    column_regs: HashMap<String, usize>,
    program: Vec<Instruction>,
}

impl Ctx {
    fn alloc(&mut self) -> usize {
        let reg = self.next_reg;
        self.next_reg = self.next_reg.saturating_add(1);
        reg
    }

    fn push(&mut self, opcode: Opcode) {
        self.program.push(Instruction::new(opcode));
    }

    fn push_commented(&mut self, opcode: Opcode, comment: impl Into<String>) {
        self.program
            .push(Instruction::with_comment(opcode, comment));
    }

    /// Load `name` once; later requests reuse the same register.
    fn load_column(&mut self, name: &str) -> usize {
        if let Some(reg) = self.column_regs.get(name) {
            return *reg;
        }
        let reg = self.alloc();
        self.push_commented(
            Opcode::LoadColumn {
                reg,
                column: name.to_string().into(),
            },
            format!("r{reg} = {name}"),
        );
        self.column_regs.insert(name.to_string(), reg);
        reg
    }
}

fn compile_expr(expr: &AstExpr, ctx: &mut Ctx) -> usize {
    // `col IN (lit, ...)`, which `parser::column` lowers to an `OR` chain
    // of `=` (an `AND` chain of `<>` for `NOT IN`): one set probe per row
    // instead of a comparison and an `OR` per item (#737).
    if let Some((column, list, negated)) = in_list_chain(expr) {
        let src = ctx.load_column(&column);
        let dst = ctx.alloc();
        ctx.push(Opcode::InList {
            dst,
            src,
            list: list.into(),
            negated,
        });
        return dst;
    }
    match &expr.kind {
        ExprKind::Column { .. } => match expr_column_name(expr) {
            Some(name) => ctx.load_column(&name),
            None => {
                let reg = ctx.alloc();
                ctx.push(Opcode::LoadConst {
                    reg,
                    value: Value::Null,
                });
                reg
            }
        },
        ExprKind::Literal(lit) => {
            let reg = ctx.alloc();
            ctx.push(Opcode::LoadConst {
                reg,
                value: literal_value(lit),
            });
            reg
        }
        // A scalar call (#307): compiled args feed `Opcode::Call`, which
        // dispatches into `crate::functions::call` at execution time.
        // `classify_item` already rejected any top-level SELECT-list call
        // this planner doesn't recognize; a call reached only through a
        // nested expression (inside `WHERE`, another call's argument, ...)
        // is not similarly validated -- an unknown name there resolves to
        // `Value::Null` at runtime ([`Opcode::Call`]'s own doc comment),
        // the same fallback every other out-of-subset shape in this
        // function gets.
        // `x ILIKE p`, which the column dialect parses as `ilike(x, p)`
        // (#679): a case-insensitive LIKE.
        ExprKind::FunctionCall {
            name,
            args: FunctionArgs::List(list),
            tail: None,
            ..
        } if name.eq_ignore_ascii_case("ilike") && list.len() == 2 => {
            let a = list.first().map_or(0, |e| compile_expr(e, ctx));
            let b = list.get(1).map_or(0, |e| compile_expr(e, ctx));
            let dst = ctx.alloc();
            ctx.push(Opcode::Map {
                dst,
                op: MapOp::Like {
                    negated: false,
                    nocase: true,
                },
                a,
                b,
            });
            dst
        }
        ExprKind::FunctionCall {
            distinct: _,
            name,
            args: FunctionArgs::List(list),
            tail,
        } if tail.as_deref().is_none_or(|t| t.over.is_none()) => {
            let arg_regs: Vec<usize> = list.iter().map(|a| compile_expr(a, ctx)).collect();
            let dst = ctx.alloc();
            ctx.push(Opcode::Call {
                dst,
                name: name.clone().into(),
                args: arg_regs.into(),
            });
            dst
        }
        ExprKind::Paren(inner) => compile_expr(inner, ctx),
        // `compile_semi_join` handles `IN (subquery)` itself and strips it
        // from `where_clause` before ever calling `compile` -- reaching
        // this arm means `IN (subquery)`/`EXISTS` was used via the regular
        // single-table path, which can't run a subquery. Compile to an
        // always-false predicate (no rows) rather than panicking.
        ExprKind::InSubquery { .. } | ExprKind::Exists { .. } => {
            let reg = ctx.alloc();
            ctx.push(Opcode::LoadConst {
                reg,
                value: Value::Bool(false),
            });
            reg
        }
        ExprKind::Binary { op, lhs, rhs } => {
            let a = compile_expr(lhs, ctx);
            let b = compile_expr(rhs, ctx);
            let dst = ctx.alloc();
            ctx.push(Opcode::Map {
                dst,
                op: map_bin_op(*op),
                a,
                b,
            });
            dst
        }
        ExprKind::Like {
            expr: inner,
            pattern,
            glob,
            negated,
            ..
        } => {
            let a = compile_expr(inner, ctx);
            let b = compile_expr(pattern, ctx);
            let dst = ctx.alloc();
            let op = if *glob {
                MapOp::Glob { negated: *negated }
            } else {
                MapOp::Like {
                    negated: *negated,
                    nocase: false,
                }
            };
            ctx.push(Opcode::Map { dst, op, a, b });
            dst
        }
        ExprKind::Unary {
            op: crate::parser::ast::UnaryOp::Not,
            expr: inner,
        } => {
            let a = compile_expr(inner, ctx);
            let dst = ctx.alloc();
            ctx.push(Opcode::Map {
                dst,
                op: MapOp::Not,
                a,
                b: a,
            });
            dst
        }
        ExprKind::Unary {
            op: crate::parser::ast::UnaryOp::Minus,
            expr: inner,
        } => {
            let a = compile_expr(inner, ctx);
            let dst = ctx.alloc();
            ctx.push(Opcode::Map {
                dst,
                op: MapOp::Neg,
                a,
                b: a,
            });
            dst
        }
        // Unary `+` is a no-op.
        ExprKind::Unary {
            op: crate::parser::ast::UnaryOp::Plus,
            expr: inner,
        } => compile_expr(inner, ctx),
        ExprKind::Unary { expr: inner, .. } => compile_expr(inner, ctx),
        ExprKind::IsNull {
            expr: inner,
            negated,
        } => {
            let a = compile_expr(inner, ctx);
            let dst = ctx.alloc();
            ctx.push(Opcode::Map {
                dst,
                op: if *negated {
                    MapOp::IsNotNull
                } else {
                    MapOp::IsNull
                },
                a,
                b: a,
            });
            dst
        }
        // `expr IS [NOT] NULL` may also parse as `Is{lhs, rhs: NULL
        // literal, negated}`.
        ExprKind::Is { lhs, rhs, negated }
            if matches!(rhs.kind, ExprKind::Literal(AstLiteral::Null)) =>
        {
            let a = compile_expr(lhs, ctx);
            let dst = ctx.alloc();
            ctx.push(Opcode::Map {
                dst,
                op: if *negated {
                    MapOp::IsNotNull
                } else {
                    MapOp::IsNull
                },
                a,
                b: a,
            });
            dst
        }
        // Anything else is outside the validated batch subset -- fall
        // back to NULL rather than panicking.
        _ => {
            let reg = ctx.alloc();
            ctx.push(Opcode::LoadConst {
                reg,
                value: Value::Null,
            });
            reg
        }
    }
}

/// Column names `expr` references, in first-encountered order (public
/// wrapper over [`collect_expr_columns`] for `engine::predicate`, #369,
/// which needs this ahead of compiling to validate against a schema).
pub fn bool_expr_columns(expr: &AstExpr) -> Vec<String> {
    let mut out = Vec::new();
    collect_expr_columns(expr, &mut out);
    out
}

/// Compiles a bare boolean expression -- not part of a `SELECT`, no table
/// scan -- into a [`Program`] that filters a single-row [`Batch`] and
/// emits no columns: run over a one-row segment, a non-empty result means
/// the row satisfied `expr` (db-core#369, `engine::predicate`). Shares
/// [`compile_expr`]'s lowering, so it accepts exactly the same expression
/// subset (including `LIKE`/`GLOB`, #352) as a `WHERE` clause does.
///
/// [`Batch`]: crate::vm::batch::Batch
pub fn compile_bool_expr(expr: &AstExpr) -> Program {
    let mut ctx = Ctx {
        next_reg: 0,
        column_regs: HashMap::new(),
        program: Vec::new(),
    };
    let predicate = compile_expr(expr, &mut ctx);
    ctx.push_commented(
        Opcode::Filter { predicate },
        format!("WHERE {}", expr_to_string(expr)),
    );
    ctx.push(Opcode::Emit {
        registers: vec![predicate].into(),
    });
    ctx.push(Opcode::Halt);
    Program::new(ctx.program)
}

/// Folds an aggregate's or window function's `FILTER (WHERE ...)` clause
/// into its source register: with no filter, `base` (the plain argument
/// column, `None` for `COUNT(*)`) passes through unchanged. With a filter,
/// compiles the predicate (loading any columns it references first, same
/// as `base`) and masks `base` -- or, for `COUNT(*) FILTER (...)`, a
/// constant `true` marker register -- to `Null` wherever the predicate is
/// false via `MapOp::MaskIf`, so `Reduce`/`GroupReduce`/`Window`'s
/// existing null-skipping does the actual filtering. Must run before
/// `WHERE`'s `Opcode::Filter`, like every other column load in `compile`/
/// `compile_window` -- see the comment where this is called.
fn mask_filtered_source(
    ctx: &mut Ctx,
    base: Option<usize>,
    filter: Option<&AstExpr>,
) -> Option<usize> {
    let Some(predicate_expr) = filter else {
        return base;
    };
    let mut cols = Vec::new();
    collect_expr_columns(predicate_expr, &mut cols);
    for name in &cols {
        ctx.load_column(name);
    }
    let predicate = compile_expr(predicate_expr, ctx);
    let data = base.unwrap_or_else(|| {
        let reg = ctx.alloc();
        ctx.push(Opcode::LoadConst {
            reg,
            value: Value::Bool(true),
        });
        reg
    });
    let masked = ctx.alloc();
    ctx.push_commented(
        Opcode::Map {
            dst: masked,
            op: MapOp::MaskIf,
            a: data,
            b: predicate,
        },
        format!("FILTER (WHERE {})", expr_to_string(predicate_expr)),
    );
    Some(masked)
}

/// Compile a flat/`GROUP BY`/`ORDER BY`/`LIMIT` query into a [`Program`]
/// ending in [`Opcode::Combine`], optionally followed by `Sort`/`Limit`
/// (db-core#48). Compiled once, reused across every segment.
pub fn compile(select: &Select) -> Result<Program> {
    if select.scope.is_some() {
        return Err(PlanError::ScopeClauseUnsupported);
    }
    // `compile_semi_join` takes a whole-WHERE `IN (SELECT ...)` off before
    // calling here; any subquery left would compile to a constant FALSE --
    // a wrong answer, not an error (#679).
    if has_subquery_condition(select) {
        return Err(PlanError::UnsupportedSemiJoin(
            "IN (SELECT ...) or EXISTS inside a condition (only a whole-WHERE `col IN (SELECT ...)` runs, through column-rs's multi-table session)".into(),
        ));
    }
    if has_range_vector_call(select) {
        return Err(PlanError::ScopeClauseUnsupported);
    }
    let mut ctx = Ctx {
        next_reg: 0,
        column_regs: HashMap::new(),
        program: Vec::new(),
    };

    let items = classify_items(select)?;
    // Display text only (plan comments); the keys are `select.group_by`'s
    // expressions, already resolved by [`expand_star`] (#623): a real
    // column by name, a `SELECT`-list alias (#307) as its expression.
    let group_by: Vec<String> = select.group_by.iter().map(group_term_label).collect();
    // #644: `HAVING` is a filter in `Combine`'s projection, over the
    // merged keys and aggregates.
    let having = select
        .having
        .as_ref()
        .map(|h| having_with_aliases(h, select));
    let having_calls: Vec<&AstExpr> = having.as_ref().map(agg_expr_calls).unwrap_or_default();
    let having_reused = having_reuse(&having_calls, select, &items)?;
    let projection = having.is_some() || needs_projection(select, &items);

    // Load every column the group-by keys and select-list aggregates need
    // *before* compiling WHERE/Filter: Filter only shrinks registers that
    // are already live, so anything loaded afterwards would keep the
    // batch's full (pre-filter) length and desync from filtered registers.
    let mut group_by_regs = Vec::new();
    for term in &select.group_by {
        group_by_regs.push(compile_expr(term, &mut ctx));
    }
    // Resolved to the final source register for each `Item::Agg` (already
    // folding in `FILTER (WHERE ...)`, if any -- see `mask_filtered_source`)
    // before `WHERE`'s `Filter` runs, so a `FILTER` predicate's own column
    // loads and its `Map { MaskIf }` masking land in the same pre-filter
    // phase as everything else here and get shrunk in lockstep by `Filter`
    // below, rather than desyncing like a load placed after it would.
    // ADR-0026: columns the WHERE clause itself references must still load
    // pre-Filter (Filter's predicate register has to be live), but a plain
    // projection column that ISN'T part of the predicate is left for the
    // second pass below (after Filter is emitted) so it's loaded only once
    // `Filter`'s `Selection` exists -- `Emit` already resolves a pending
    // `Selection` lazily against any full-length register, pre- or
    // post-Filter (see `Opcode::Emit`), so a `LoadColumn` emitted after
    // `Filter` needs no special handling there.
    let mut where_columns: Vec<String> = Vec::new();
    if let Some(where_clause) = &select.where_clause {
        collect_expr_columns(where_clause, &mut where_columns);
    }

    // One entry per source register an `Item::Agg`/`Item::AggExpr` needs:
    // a single `Option<usize>` for a plain aggregate, one per nested
    // aggregate call (in `agg_expr_calls`' left-to-right order) for #496's
    // `AggExpr`, empty otherwise.
    let mut agg_srcs: Vec<Vec<Option<usize>>> = Vec::new();
    for item in &items {
        match item {
            Item::Agg(_, arg, filter, distinct) => {
                let base = arg.as_ref().map(|a| match a {
                    // `COUNT(DISTINCT x)`'s `x` becomes a group key, which
                    // `GroupReduce` overwrites with the groups' keys: it
                    // gets a register of its own, not one an aggregate or
                    // a real key also reads (#633).
                    AggArg::Column(name) if *distinct => {
                        let reg = ctx.alloc();
                        ctx.push_commented(
                            Opcode::LoadColumn {
                                reg,
                                column: name.clone().into(),
                            },
                            format!("r{reg} = {name} (COUNT DISTINCT key)"),
                        );
                        reg
                    }
                    AggArg::Column(name) => ctx.load_column(name),
                    AggArg::Expr(expr) => compile_expr(expr, &mut ctx),
                });
                agg_srcs.push(vec![mask_filtered_source(&mut ctx, base, filter.as_ref())]);
            }
            // #496: each nested aggregate call's own argument is resolved
            // here, pre-`Filter`, for the same reason `Item::Agg`'s is
            // above -- `compile`'s second pass only allocates registers
            // and `AggPart`s from these already-resolved sources, it
            // never loads a column itself.
            Item::AggExpr(expr) => {
                let srcs = agg_expr_calls(expr)
                    .into_iter()
                    .map(|call| agg_call_source(call, &mut ctx))
                    .collect::<Result<Vec<_>>>()?;
                agg_srcs.push(srcs);
            }
            // Plain projected columns are emitted (not aggregated). A
            // column the WHERE clause also reads must be loaded here for
            // the same reason as the keys above: a column first loaded
            // below the Filter keeps its full pre-filter length while the
            // filtered registers shrink, and Emit then indexes past the end
            // of the short ones. A projection-only column (not referenced
            // by WHERE) is deliberately left unloaded here -- it's loaded
            // by the second pass further down, after `Filter` is emitted
            // (ADR-0026, late materialization). `load_column` memoizes, so
            // either way the projection code further down reuses whichever
            // register this loop already created instead of emitting a
            // second LoadColumn.
            Item::Column(name) if group_by.is_empty() => {
                if where_columns.iter().any(|c| c == name) {
                    ctx.load_column(name);
                }
                agg_srcs.push(Vec::new());
            }
            Item::Expr(expr) if group_by.is_empty() => {
                let mut cols = Vec::new();
                collect_expr_columns(expr, &mut cols);
                for name in &cols {
                    if where_columns.iter().any(|c| c == name) {
                        ctx.load_column(name);
                    }
                }
                agg_srcs.push(Vec::new());
            }
            _ => agg_srcs.push(Vec::new()),
        }
    }

    // The sources of the aggregates only `HAVING` reads, loaded with the
    // select list's, before `WHERE`'s `Filter`.
    let having_srcs = having_calls
        .iter()
        .zip(&having_reused)
        .map(|(call, reused)| match reused {
            Some(_) => Ok(None),
            None => agg_call_source(call, &mut ctx),
        })
        .collect::<Result<Vec<_>>>()?;

    if let Some(where_clause) = &select.where_clause {
        let predicate = compile_expr(where_clause, &mut ctx);
        ctx.push_commented(
            Opcode::Filter { predicate },
            format!("WHERE {}", expr_to_string(where_clause)),
        );
    }

    let mut agg_parts = Vec::new();
    for _ in &group_by {
        agg_parts.push(AggPart::GroupKey);
    }
    // #633: `COUNT(DISTINCT x)` groups by `x` as one more, hidden key; see
    // `AggPart::DistinctKey`. One per query: a second distinct argument
    // would need a second grouping.
    let mut distinct_srcs = items
        .iter()
        .zip(&agg_srcs)
        .filter(|(item, _)| matches!(item, Item::Agg(_, _, _, true)))
        .map(|(_, src)| src.first().copied().flatten());
    let distinct_src = distinct_srcs.next();
    if distinct_srcs.next().is_some() {
        return Err(PlanError::UnsupportedSelectItem(
            "more than one COUNT(DISTINCT ...) in a query".into(),
        ));
    }
    if let Some(src) = distinct_src {
        let src = src
            .ok_or_else(|| PlanError::Internal("COUNT(DISTINCT ...) without an argument".into()))?;
        group_by_regs.push(src);
        agg_parts.push(AggPart::DistinctKey);
    }
    let num_group_keys = group_by_regs.len();

    let mut aggs: Vec<(AggFunc, Option<usize>)> = Vec::new();
    let mut agg_dst = Vec::new();
    let mut emit_regs = group_by_regs.clone();
    // Per select item, the merged output column of each aggregate it uses
    // (one for `Item::Agg`, one per call for `Item::AggExpr`), for the
    // post-aggregation projection (#623).
    let mut item_slots: Vec<Vec<usize>> = Vec::with_capacity(items.len());

    for (item, agg_src) in items.iter().zip(&agg_srcs) {
        let mut slots = Vec::new();
        if let Item::Agg(_, _, _, true) = item {
            // Its value is the hidden key's count, emitted with the keys
            // and merged right after them.
            slots.push(group_by.len());
        } else if let Item::Agg(func, _, _, false) = item {
            let src = agg_src.first().copied().flatten();
            slots.push(push_aggregate(
                *func,
                src,
                &mut aggs,
                &mut agg_dst,
                &mut agg_parts,
                &mut emit_regs,
                &mut ctx,
            ));
        } else if let Item::Column(name) = item {
            // A plain column in the SELECT list: without GROUP BY it isn't
            // loaded or emitted anywhere else yet, so load and emit it
            // here. With GROUP BY it is a key, already in `emit_regs` via
            // `group_by_regs`, and the projection (#623) places it.
            if group_by.is_empty() {
                let reg = ctx.load_column(name);
                emit_regs.push(reg);
            }
        } else if let Item::Expr(expr) = item {
            // Same rule as a plain column above: only emitted here without
            // GROUP BY. With one, it is a key (#307) or the projection
            // computes it from the keys (#623). Its referenced columns are
            // already loaded pre-Filter, so `compile_expr` here reuses
            // those (correctly filtered) registers.
            if group_by.is_empty() {
                let reg = compile_expr(expr, &mut ctx);
                emit_regs.push(reg);
            }
        } else if let Item::AggExpr(expr) = item {
            // #496/#623: each aggregate call is merged as an ordinary
            // output column; the projection computes the expression over
            // them, so any operator, nesting or literal works, and
            // literals keep their type.
            for (call, src) in agg_expr_calls(expr).into_iter().zip(agg_src) {
                let ExprKind::FunctionCall { name, .. } = &call.kind else {
                    return Err(PlanError::Internal(
                        "agg_expr_calls returned a non-FunctionCall node".into(),
                    ));
                };
                let func = AggFunc::from_name(name).unwrap_or(AggFunc::Count);
                slots.push(push_aggregate(
                    func,
                    *src,
                    &mut aggs,
                    &mut agg_dst,
                    &mut agg_parts,
                    &mut emit_regs,
                    &mut ctx,
                ));
            }
        }
        item_slots.push(slots);
    }
    // Each `HAVING` aggregate's merged column: the select list's, or one
    // added here that the projection reads but never emits.
    let mut having_slots = Vec::with_capacity(having_calls.len());
    for ((call, reused), src) in having_calls.iter().zip(&having_reused).zip(&having_srcs) {
        let slot = match reused {
            Some((i, k)) => item_slots.get(*i).and_then(|slots| slots.get(*k)).copied(),
            None => {
                let ExprKind::FunctionCall { name, .. } = &call.kind else {
                    return Err(PlanError::Internal(
                        "agg_expr_calls returned a non-FunctionCall node".into(),
                    ));
                };
                let func = AggFunc::from_name(name).unwrap_or(AggFunc::Count);
                Some(push_aggregate(
                    func,
                    *src,
                    &mut aggs,
                    &mut agg_dst,
                    &mut agg_parts,
                    &mut emit_regs,
                    &mut ctx,
                ))
            }
        };
        having_slots.push(slot.ok_or_else(|| {
            PlanError::Internal("a HAVING aggregate without a merged column".into())
        })?);
    }

    if group_by_regs.is_empty() {
        // #452: an aggregate with no GROUP BY is a global reduction, one
        // `Reduce` per aggregate -- not a `GroupReduce` with zero key
        // columns. The keyless `GroupReduce` cloned every aggregated column
        // into a single group before reducing it, and, over zero surviving
        // rows, found zero groups and emitted *no row* where SQL requires
        // exactly one (`COUNT(*)` = 0, everything else NULL). `Reduce`
        // always writes one row, and is the opcode #433's typed `Column`
        // fast path lives on.
        for ((func, src), dst) in aggs.iter().zip(agg_dst.iter()) {
            ctx.push_commented(
                Opcode::Reduce {
                    func: *func,
                    src: *src,
                    dst: *dst,
                },
                "aggregate",
            );
        }
    } else {
        ctx.push_commented(
            Opcode::GroupReduce {
                group_by: group_by_regs.into(),
                aggs: aggs.into(),
                agg_dst: agg_dst.into(),
            },
            if group_by.is_empty() {
                "COUNT(DISTINCT ...)".to_string()
            } else {
                format!("GROUP BY {}", group_by.join(", "))
            },
        );
    }

    // `ORDER BY` (#645), resolved before `Emit`: a query that neither
    // groups nor aggregates may sort on a column it does not select,
    // emitted after the list.
    let sort = order_sort(
        select,
        agg_parts.is_empty().then_some((&mut ctx, &mut emit_regs)),
    )?;
    ctx.push_commented(
        Opcode::Emit {
            registers: emit_regs.into(),
        },
        format!("SELECT {}", output_column_names(select).join(", ")),
    );

    let limit = select_limit(select);
    let has_agg = classify_items(select)?
        .iter()
        .any(|c| matches!(c, Item::Agg(..) | Item::AggExpr(_)));
    let group_by_present = !group_by.is_empty();

    // `Combine` is always emitted, mirroring the old bundled `Finalize`
    // (db-core#48): even a plain `SELECT` with no aggregate/`ORDER BY`/
    // `LIMIT` goes through the merge phase, which is a no-op
    // concatenation when `agg_parts` is empty. `Sort`/`Limit` follow
    // only when the query actually has them, so a program with neither
    // still ends in a bare `Combine`, not `Combine, Sort, Limit` with
    // both absent.
    let combine_comment = if group_by_present || has_agg {
        "merge partial aggregates".to_string()
    } else {
        "concatenate segments".to_string()
    };
    ctx.push_commented(
        Opcode::Combine {
            agg_parts: agg_parts.into(),
            num_group_keys,
            distinct: matches!(select.distinct, Some(Distinctness::Distinct)),
            output_order: if projection {
                Vec::new()
            } else {
                grouped_output_order(&items, group_by.len())
            }
            .into(),
            project: if projection {
                let having = having
                    .as_ref()
                    .map(|h| (h, &having_calls[..], &having_slots[..]));
                build_projection(select, &items, &item_slots, having)?
            } else {
                Vec::new()
            }
            .into(),
        },
        combine_comment,
    );
    if let Some((sort, comment)) = sort {
        ctx.push_commented(sort, comment);
    }
    if let Some(n) = limit {
        let offset = select_offset(select);
        let comment = match offset {
            0 => format!("LIMIT {n}"),
            m => format!("LIMIT {n} OFFSET {m}"),
        };
        ctx.push_commented(Opcode::Limit { n, offset }, comment);
    }

    Ok(Program::new(ctx.program))
}

/// Whether `select`'s WHERE, HAVING or select list holds an `IN (SELECT
/// ...)` or `EXISTS` (#679): the single-table plan cannot run one, and
/// used to compile it to FALSE.
pub fn has_subquery_condition(select: &Select) -> bool {
    let items = select.columns.iter().filter_map(|col| match col {
        ResultColumn::Expr { expr, .. } => Some(expr),
        _ => None,
    });
    select
        .where_clause
        .iter()
        .chain(select.having.iter())
        .chain(items)
        .any(expr_has_subquery)
}

fn expr_has_subquery(expr: &AstExpr) -> bool {
    match &expr.kind {
        ExprKind::InSubquery { .. } | ExprKind::Exists { .. } => true,
        ExprKind::Binary { lhs, rhs, .. }
        | ExprKind::Is { lhs, rhs, .. }
        | ExprKind::Like {
            expr: lhs,
            pattern: rhs,
            ..
        } => expr_has_subquery(lhs) || expr_has_subquery(rhs),
        ExprKind::Unary { expr: inner, .. }
        | ExprKind::IsNull { expr: inner, .. }
        | ExprKind::Paren(inner) => expr_has_subquery(inner),
        ExprKind::FunctionCall {
            args: FunctionArgs::List(list),
            ..
        } => list.iter().any(expr_has_subquery),
        _ => false,
    }
}

/// Split a (possibly qualified) column name into `(table_prefix, column)`.
pub fn split_qualified(name: &str) -> (Option<&str>, &str) {
    match name.split_once('.') {
        Some((table, column)) => (Some(table), column),
        None => (None, name),
    }
}

/// Which physical source backs a [`compile_join`] build (right/`JOIN`)
/// side (ADR 0024, #382/#386). `compile_join` classifies a query purely
/// from its `Select` AST -- it has no way to tell a SQLite lookup table
/// from a stream table or an already-materialized batch on its own -- so
/// the caller supplies this, having already applied ADR-0019/ADR-0022's
/// fixed build-side rule (the lookup/`JOIN`-target side always builds,
/// never chosen by cost).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildSourceKind {
    /// Build side is a SQLite table read through `engine::row` (ADR-0019,
    /// `engine::cross_mode`).
    RowTable,
    /// Build side is windowed stream segments (ADR-0022).
    Stream,
    /// Build side is already an in-memory `Batch` -- today's only
    /// `compile_join` callers outside cross-mode queries (a plain
    /// table-to-table batch join, and this module's own unit tests).
    InMemory,
}

/// Plan a query with exactly one `JOIN` (INNER or LEFT; only the first
/// join clause is honored -- chained/multi-way joins aren't supported).
/// An unqualified column name is assumed to belong to the `FROM` table; a
/// right-table column must be qualified (`table.column`) to disambiguate.
///
/// Build side: every right-table column is loaded into registers
/// `0..len`, then hashed on the join key (all registers doubling as
/// payload, so the key column itself is also NULL-filled on an unmatched
/// `LEFT JOIN` probe row, same as any other right-table column). Probe
/// side: every left-table column is loaded, then the hash table is probed,
/// landing the right side's payload right after the left columns.
pub fn compile_join(select: &Select, build_source: BuildSourceKind) -> Result<JoinProgram> {
    compile_join_impl(select, None, build_source)
}

/// Like [`compile_join`], but `build_table` names the table that must
/// build the hash side regardless of whether it's written as the `FROM`
/// table or the `JOIN` target (ADR-0021) -- used by cross-mode joins
/// (`engine::resolve`), where the SQLite lookup side must always build
/// (ADR-0019) even when the query names it first (`hosts JOIN log`, not
/// just `log JOIN hosts`).
///
/// Only sound for `INNER JOIN` when `build_table` names the `FROM` table:
/// a `LEFT JOIN`'s probe side is always the one whose unmatched rows are
/// kept, and that must stay the driving/stream side (probe execution is
/// fixed to the stream segments by [`crate::vm::engine::run_join_segments`]'s
/// calling convention) -- swapping which side keeps unmatched rows would
/// require a `RIGHT JOIN`-shaped algorithm, which isn't implemented.
/// Returns [`PlanError::UnsupportedJoinKind`] for that combination.
pub fn compile_join_build_side(
    select: &Select,
    build_table: &str,
    build_source: BuildSourceKind,
) -> Result<JoinProgram> {
    compile_join_impl(select, Some(build_table), build_source)
}

fn compile_join_impl(
    select: &Select,
    build_override: Option<&str>,
    build_source: BuildSourceKind,
) -> Result<JoinProgram> {
    let Some(from) = &select.from else {
        return Err(PlanError::UnknownColumn("SELECT without FROM".into()));
    };
    let joins = extract_joins(from)?;
    let Some(join) = joins.first() else {
        return Err(PlanError::NoJoinClause);
    };
    if !matches!(join.op, JoinOp::Inner | JoinOp::Left) {
        return Err(PlanError::UnsupportedJoinKind(join.op));
    }
    let from_name = table_name(from)?;
    let select = &with_extra_key_filters(select, from_name, &joins)?;
    let body = compile(select)?;

    let mut needed: Vec<String> = body.columns_to_load();
    for extra in [&join.left_col, &join.right_col] {
        if !needed.contains(extra) {
            needed.push(extra.clone());
        }
    }

    let from_name = table_name(from)?;
    let mut from_columns = Vec::new();
    let mut join_columns = Vec::new();
    for name in &needed {
        let (prefix, _) = split_qualified(name);
        match prefix {
            None => from_columns.push(name.clone()),
            Some(p) if p == from_name => from_columns.push(name.clone()),
            Some(p) if p == join.table => join_columns.push(name.clone()),
            Some(_) => return Err(PlanError::UnknownColumn(name.clone())),
        }
    }

    // By default (and always for a plain `compile_join` call) the JOIN
    // target builds, the FROM table probes -- today's position-based rule.
    // `build_override` swaps this when the caller names the FROM table as
    // the side that must build.
    let from_builds = match build_override {
        None => false,
        Some(name) if name == join.table => false,
        Some(name) if name == from_name => true,
        Some(_) => {
            return Err(PlanError::Internal(
                "compile_join_build_side: build_table names neither the \
                 FROM table nor the JOIN target"
                    .into(),
            ))
        }
    };
    if from_builds && join.op != JoinOp::Inner {
        return Err(PlanError::UnsupportedJoinKind(join.op));
    }

    let (build_columns, build_key, probe_columns, probe_key) = if from_builds {
        (from_columns, &join.left_col, join_columns, &join.right_col)
    } else {
        (join_columns, &join.right_col, from_columns, &join.left_col)
    };

    let build_key_reg = build_columns
        .iter()
        .position(|n| n == build_key)
        .ok_or_else(|| PlanError::UnknownColumn(build_key.clone()))?;
    // `ScanSource` names where the build side comes from (ADR 0024,
    // #382/#386) -- a no-op to the VM (the batch it operates on still
    // arrives however the caller resolves it, `vm::engine::run_join_segments`
    // (#385)), but a real, visible first step in the emitted program so
    // `explain_opcodes` shows a cross-mode join's build side instead of the
    // "not available" gap this ADR closes. `InMemory` carries an empty
    // placeholder `Batch`: the real one is resolved at execution time
    // (`ScanSourceResolver`), never read back out of this opcode. The
    // build table's name is whichever side actually builds (ADR-0021's
    // `from_builds` may swap it from the `JOIN` target to the `FROM` table).
    let build_table_name = if from_builds {
        from_name
    } else {
        join.table.as_str()
    };
    let scan_source = match build_source {
        BuildSourceKind::RowTable => Opcode::ScanSource(ScanSource::RowTable {
            table: Cow::Owned(build_table_name.to_string()),
            columns: build_columns
                .iter()
                .map(|n| Cow::Owned(split_qualified(n).1.to_string()))
                .collect(),
        }),
        BuildSourceKind::Stream => Opcode::ScanSource(ScanSource::Stream {
            handle: 0,
            columns: build_columns
                .iter()
                .map(|n| Cow::Owned(n.clone()))
                .collect(),
            #[cfg(feature = "vm-stream")]
            scope: None,
        }),
        BuildSourceKind::InMemory => {
            Opcode::ScanSource(ScanSource::InMemory(crate::vm::batch::Batch::default()))
        }
    };
    let build = Program::from_opcodes(
        std::iter::once(scan_source)
            .chain(
                build_columns
                    .iter()
                    .enumerate()
                    .map(|(reg, name)| Opcode::LoadColumn {
                        reg,
                        column: name.clone().into(),
                    }),
            )
            .chain(std::iter::once(Opcode::HashBuild {
                key_cols: vec![build_key_reg].into(),
                payload_cols: (0..build_columns.len()).collect::<Vec<_>>().into(),
                table: 0,
            })),
    );

    let probe_key_reg = probe_columns
        .iter()
        .position(|n| n == probe_key)
        .ok_or_else(|| PlanError::UnknownColumn(probe_key.clone()))?;
    let payload_dst: Vec<usize> = (0..build_columns.len())
        .map(|i| probe_columns.len().saturating_add(i))
        .collect();
    // Already rejected above (either at the top of this function, or by
    // the `from_builds && join.op != Inner` check); returning the same
    // error here keeps this match total without an `unreachable!` the
    // qualified subset (`make check-mvl-limit`) forbids.
    let join_kind = match join.op {
        JoinOp::Inner => crate::vm::batch::JoinKind::Inner,
        JoinOp::Left => crate::vm::batch::JoinKind::Left,
        other => return Err(PlanError::UnsupportedJoinKind(other)),
    };
    let fused = try_fuse_group_by(
        &body,
        &probe_columns,
        &build_columns,
        probe_key_reg,
        join_kind,
    );
    let probe_load = probe_columns
        .iter()
        .enumerate()
        .map(|(reg, name)| Opcode::LoadColumn {
            reg,
            column: name.clone().into(),
        });
    let (probe, body, fused_group_by) = match fused {
        Some((fused_op, fused_output, trimmed_body)) => (
            Program::from_opcodes(probe_load.chain(std::iter::once(fused_op))),
            trimmed_body,
            Some(fused_output),
        ),
        None => (
            Program::from_opcodes(probe_load.chain(std::iter::once(Opcode::HashProbe {
                key_cols: vec![probe_key_reg].into(),
                table: 0,
                payload_dst: payload_dst.clone().into(),
                kind: join_kind,
            }))),
            body,
            None,
        ),
    };

    Ok(JoinProgram {
        left_columns: probe_columns,
        right_columns: build_columns,
        build,
        probe,
        payload_dst,
        body,
        fused_group_by,
    })
}

/// #441: detects whether `body` is exactly `[LoadColumn]* GroupReduce
/// [suffix]` -- no `Map`/`Filter`/`Window` between the last `LoadColumn`
/// and the `GroupReduce` -- meaning the join's output feeds only a
/// `GROUP BY`/aggregate and nothing else touches the joined row first.
/// When it is, returns the `Opcode::HashProbeGroupReduce` to run instead
/// of `Opcode::HashProbe`, the `(register, synthetic name)` pairs its
/// `group_by`/`agg_dst` registers should be read back through (see
/// [`JoinProgram::fused_group_by`]), and `body`'s trimmed suffix (every
/// opcode after the `GroupReduce`, unchanged). Returns `None` for every
/// other shape -- including a body with no `GroupReduce` at all (a plain,
/// non-aggregate join) -- which keeps that join on the existing unfused
/// path with byte-for-byte identical `probe`/`body` programs.
type FusedGroupBy = (Opcode, Vec<(usize, String)>, Program);

fn try_fuse_group_by(
    body: &Program,
    probe_columns: &[String],
    build_columns: &[String],
    probe_key_reg: usize,
    join_kind: crate::vm::batch::JoinKind,
) -> Option<FusedGroupBy> {
    let ops: Vec<&Opcode> = body.opcodes().collect();
    let mut name_by_reg: HashMap<usize, String> = HashMap::new();
    let mut idx = 0;
    while let Some(Opcode::LoadColumn { reg, column }) = ops.get(idx) {
        name_by_reg.insert(*reg, column.to_string());
        idx = idx.saturating_add(1);
    }
    let Some(Opcode::GroupReduce {
        group_by,
        aggs,
        agg_dst,
    }) = ops.get(idx)
    else {
        return None;
    };

    let resolve_source = |reg: usize| -> Option<ValueSource> {
        let name = name_by_reg.get(&reg)?;
        if let Some(pos) = probe_columns.iter().position(|c| c == name) {
            Some(ValueSource::Probe(pos))
        } else {
            build_columns
                .iter()
                .position(|c| c == name)
                .map(ValueSource::Payload)
        }
    };

    let mut fused_group_by: Vec<(ValueSource, usize)> = Vec::with_capacity(group_by.len());
    for reg in group_by.iter() {
        fused_group_by.push((resolve_source(*reg)?, *reg));
    }
    let mut fused_aggs: Vec<(AggFunc, Option<ValueSource>)> = Vec::with_capacity(aggs.len());
    for (func, src) in aggs.iter() {
        let resolved = match src {
            Some(reg) => Some(resolve_source(*reg)?),
            None => None,
        };
        fused_aggs.push((*func, resolved));
    }

    let fused_op = Opcode::HashProbeGroupReduce {
        key_cols: vec![probe_key_reg].into(),
        table: 0,
        kind: join_kind,
        group_by: fused_group_by.into(),
        aggs: fused_aggs.into(),
        agg_dst: agg_dst.clone(),
    };

    let fused_output: Vec<(usize, String)> = group_by
        .iter()
        .enumerate()
        .map(|(i, reg)| (*reg, format!("__fused_group_{i}")))
        .chain(
            agg_dst
                .iter()
                .enumerate()
                .map(|(i, reg)| (*reg, format!("__fused_agg_{i}"))),
        )
        .collect();

    let reload = fused_output
        .iter()
        .map(|(reg, name)| Opcode::LoadColumn {
            reg: *reg,
            column: name.clone().into(),
        })
        .chain(
            ops.get(idx.saturating_add(1)..)
                .into_iter()
                .flatten()
                .map(|op| (*op).clone()),
        );
    let trimmed_body = Program::from_opcodes(reload);

    Some((fused_op, fused_output, trimmed_body))
}

/// Plan a cross-mode star join (#394): the driving table (`from.first`)
/// joined to N SQLite lookup tables, one per `JOIN` clause, e.g. `log JOIN
/// hosts ON log.host_id = hosts.id JOIN users ON log.user_id = users.id`.
/// Unlike [`compile_join`]/[`compile_join_build_side`], the driving side
/// never swaps position and always probes: every join key must be a column
/// of the driving table (`engine::resolve::resolve_multi_sides` enforces
/// this before calling in), so every SQLite lookup builds independently --
/// no lookup table is ever joined against another lookup table's payload.
/// `lookup_tables` names each `JOIN` target's table, in query/`JOIN` order
/// (one entry per join; `resolve_multi_sides` has already confirmed each is
/// a known SQLite table).
///
/// Only `INNER`/`LEFT` per join (same restriction as [`compile_join`]); a
/// single `JOIN` is also accepted here (equivalent to
/// `compile_join_build_side` with the JOIN target as the build table).
pub fn compile_cross_mode_multi_join(
    select: &Select,
    lookup_tables: &[String],
) -> Result<crate::vm::engine::MultiJoinProgram> {
    let Some(from) = &select.from else {
        return Err(PlanError::UnknownColumn("SELECT without FROM".into()));
    };
    let joins = extract_joins(from)?;
    if joins.is_empty() {
        return Err(PlanError::NoJoinClause);
    }
    if joins.len() != lookup_tables.len() {
        return Err(PlanError::Internal(
            "compile_cross_mode_multi_join: lookup_tables must have exactly \
             one entry per JOIN clause"
                .into(),
        ));
    }
    for (join, table) in joins.iter().zip(lookup_tables) {
        if !join.table.eq_ignore_ascii_case(table) {
            return Err(PlanError::Internal(format!(
                "compile_cross_mode_multi_join: lookup_tables[{table}] does \
                 not match JOIN target `{}`",
                join.table
            )));
        }
        if !matches!(join.op, JoinOp::Inner | JoinOp::Left) {
            return Err(PlanError::UnsupportedJoinKind(join.op));
        }
    }

    let from_name = table_name(from)?;
    let select = &with_extra_key_filters(select, from_name, &joins)?;
    let body = compile(select)?;

    let mut needed: Vec<String> = body.columns_to_load();
    for j in &joins {
        for extra in [&j.left_col, &j.right_col] {
            if !needed.contains(extra) {
                needed.push(extra.clone());
            }
        }
    }

    // Split the needed columns into the driving side's columns (probe,
    // register order below) and each join's own build-side columns --
    // mirrors `compile_join_impl`'s binary split, generalized to N sides.
    let mut from_columns: Vec<String> = Vec::new();
    let mut join_columns: Vec<Vec<String>> = vec![Vec::new(); joins.len()];
    for name in &needed {
        let (prefix, _) = split_qualified(name);
        let owner = match prefix {
            None => None,
            Some(p) if p == from_name => None,
            Some(p) => joins.iter().position(|j| j.table == p),
        };
        match owner {
            None => from_columns.push(name.clone()),
            Some(i) => join_columns
                .get_mut(i)
                .ok_or_else(|| PlanError::UnknownColumn(name.clone()))?
                .push(name.clone()),
        }
    }

    // Which of `left_col`/`right_col` belongs to the driving side (the
    // probe key) vs. this join's own table (the build key) -- checked by
    // membership rather than assumed lhs/rhs order, since each join's `ON`
    // may write either side first.
    fn owner_of(col: &str, from_name: &str, join_table: &str) -> Result<&'static str> {
        let (prefix, _) = split_qualified(col);
        match prefix {
            None => Ok("from"),
            Some(p) if p == from_name => Ok("from"),
            Some(p) if p == join_table => Ok("join"),
            Some(_) => Err(PlanError::UnknownColumn(col.to_string())),
        }
    }

    let probe_reg_of: HashMap<&str, usize> = from_columns
        .iter()
        .enumerate()
        .map(|(i, n)| (n.as_str(), i))
        .collect();

    let mut builds = Vec::with_capacity(joins.len());
    let mut probe_ops: Vec<Opcode> = from_columns
        .iter()
        .enumerate()
        .map(|(reg, name)| Opcode::LoadColumn {
            reg,
            column: name.clone().into(),
        })
        .collect();
    let mut next_payload_reg = from_columns.len();

    for (i, join) in joins.iter().enumerate() {
        let left_owner = owner_of(&join.left_col, from_name, &join.table)?;
        let right_owner = owner_of(&join.right_col, from_name, &join.table)?;
        let (probe_key, build_key) = match (left_owner, right_owner) {
            ("from", "join") => (&join.left_col, &join.right_col),
            ("join", "from") => (&join.right_col, &join.left_col),
            _ => {
                return Err(PlanError::UnknownColumn(format!(
                    "JOIN ON must equate the driving table to `{}`",
                    join.table
                )))
            }
        };

        let build_columns = join_columns.get(i).cloned().unwrap_or_default();
        let build_key_reg = build_columns
            .iter()
            .position(|n| n == build_key)
            .ok_or_else(|| PlanError::UnknownColumn(build_key.clone()))?;

        let scan_source = Opcode::ScanSource(ScanSource::RowTable {
            table: Cow::Owned(join.table.clone()),
            columns: build_columns
                .iter()
                .map(|n| Cow::Owned(split_qualified(n).1.to_string()))
                .collect(),
        });
        let build = Program::from_opcodes(
            std::iter::once(scan_source)
                .chain(
                    build_columns
                        .iter()
                        .enumerate()
                        .map(|(reg, name)| Opcode::LoadColumn {
                            reg,
                            column: name.clone().into(),
                        }),
                )
                .chain(std::iter::once(Opcode::HashBuild {
                    key_cols: vec![build_key_reg].into(),
                    payload_cols: (0..build_columns.len()).collect::<Vec<_>>().into(),
                    table: i,
                })),
        );

        let probe_key_reg = *probe_reg_of
            .get(probe_key.as_str())
            .ok_or_else(|| PlanError::UnknownColumn(probe_key.clone()))?;
        let payload_dst: Vec<usize> = (0..build_columns.len())
            .map(|k| next_payload_reg.saturating_add(k))
            .collect();
        next_payload_reg = next_payload_reg.saturating_add(build_columns.len());

        let join_kind = match join.op {
            JoinOp::Inner => crate::vm::batch::JoinKind::Inner,
            JoinOp::Left => crate::vm::batch::JoinKind::Left,
            other => return Err(PlanError::UnsupportedJoinKind(other)),
        };
        probe_ops.push(Opcode::HashProbe {
            key_cols: vec![probe_key_reg].into(),
            table: i,
            payload_dst: payload_dst.clone().into(),
            kind: join_kind,
        });

        builds.push(crate::vm::engine::JoinBuildSide {
            table_name: join.table.clone(),
            right_columns: build_columns,
            build,
            payload_dst,
        });
    }

    Ok(crate::vm::engine::MultiJoinProgram {
        left_columns: from_columns,
        builds,
        probe: Program::from_opcodes(probe_ops),
        body,
    })
}

/// A planned `WHERE col IN (SELECT ...)` semi-join: the caller plans and
/// runs `subquery` via [`compile`] on its own table, collects the allowed
/// key set, filters the main table on `key_column` (see
/// [`crate::vm::engine::semi_filter`]), and runs `body` over the survivors.
/// `body` is the main query compiled with the `IN` clause stripped -- the
/// subquery isn't a VM predicate.
/// Owns `subquery` (cloned out of the `IN` clause's already-boxed
/// `Select`) rather than borrowing it -- this codebase's qualified
/// subset (`make check-mvl-limit`) forbids the explicit lifetime a
/// borrowing `SemiJoinProgram<'q>` would need.
#[derive(Debug, Clone, PartialEq)]
pub struct SemiJoinProgram {
    /// The main-table column tested by the `IN` clause.
    pub key_column: String,
    /// The `IN (SELECT ...)` subquery, to be compiled and run by the caller.
    pub subquery: Box<Select>,
    /// The main query with the `IN` clause stripped, run over the filtered rows.
    pub body: Program,
}

/// Plan a query whose entire `WHERE` clause is `col IN (SELECT ...)`.
/// Combining the `IN` clause with other conditions via `AND`/`OR` isn't
/// supported -- the semi-join must be the whole `WHERE` clause.
pub fn compile_semi_join(select: &Select) -> Result<SemiJoinProgram> {
    let Some(AstExpr {
        kind:
            ExprKind::InSubquery {
                expr,
                subquery,
                negated: false,
            },
        ..
    }) = &select.where_clause
    else {
        return Err(PlanError::UnsupportedSemiJoin(
            "WHERE clause must be exactly `col IN (SELECT ...)`".to_string(),
        ));
    };
    let key_column = expr_column_name(expr).ok_or_else(|| {
        PlanError::UnsupportedSemiJoin("IN's left-hand side must be a bare column".to_string())
    })?;

    let mut stripped = select.clone();
    stripped.where_clause = None;
    Ok(SemiJoinProgram {
        key_column,
        subquery: subquery.clone(),
        body: compile(&stripped)?,
    })
}

/// Plan a query whose `SELECT` list contains one or more window functions
/// (`ROW_NUMBER`/`RANK`/`DENSE_RANK`, `LAG`/`LEAD`, `FIRST_VALUE`/
/// `LAST_VALUE`, `SUM`/`AVG`/`COUNT OVER`). Window functions need the
/// whole table materialized (partitioning and sorting happen over the
/// entire input), so the caller runs this program over a single in-memory
/// segment. `WHERE` filters the rows before the windows see them, as in
/// DuckDB (#679: it used to be dropped). Plain aggregates aren't supported
/// combined with window functions in this minimal implementation -- only
/// plain columns and window items in `SELECT`; an aggregate or `*` item
/// emits NULL.
///
/// `LAST_VALUE`'s default frame (`RANGE UNBOUNDED PRECEDING .. CURRENT
/// ROW`, per the SQL standard when `ORDER BY` is present in `OVER`) makes
/// it return the *current* row's value, not the partition's true last row
/// -- that's implemented literally, ignoring `RANGE` peer-group ties.
pub fn compile_window(select: &Select) -> Result<Program> {
    let items = classify_items(select)?;

    let mut needed: Vec<String> = Vec::new();
    let push_needed = |name: &str, needed: &mut Vec<String>| {
        if !needed.iter().any(|n| n == name) {
            needed.push(name.to_string());
        }
    };
    // Collect every column any item actually references.
    for item in &items {
        match item {
            Item::Column(name) => push_needed(name, &mut needed),
            Item::Window(spec) => {
                if let Some(arg) = &spec.arg {
                    push_needed(arg, &mut needed);
                }
                for p in &spec.partition_by {
                    push_needed(p, &mut needed);
                }
                for (o, _) in &spec.order_by {
                    push_needed(o, &mut needed);
                }
            }
            Item::Agg(..) | Item::Star | Item::Expr(_) | Item::AggExpr(_) => {}
        }
    }

    // `needed[i]` is loaded into register `i`; each `Opcode::Window`
    // writes its result into a fresh register past those.
    // `needed` is built from the same window specs a few lines above, so
    // the `Err` is unreachable -- kept typed rather than `expect`ed
    // (db-core#231).
    let column_reg = |name: &str| -> Result<usize> {
        needed.iter().position(|n| n == name).ok_or_else(|| {
            PlanError::UnsupportedSelectItem(format!(
                "window column {name} is not in the load list"
            ))
        })
    };

    let program: Vec<Instruction> = needed
        .iter()
        .enumerate()
        .map(|(reg, name)| {
            Instruction::with_comment(
                Opcode::LoadColumn {
                    reg,
                    column: name.clone().into(),
                },
                format!("r{reg} = {name}"),
            )
        })
        .collect();

    // Resolve each window item's `FILTER (WHERE ...)` clause (Sum/Avg/Count
    // only, enforced in `classify_item`) into a masked source register,
    // reusing `Ctx`/`mask_filtered_source` even though this function
    // otherwise addresses registers by `needed`'s fixed positions rather
    // than `Ctx`'s memoizing `load_column` -- a filter predicate's own
    // column loads simply land past `needed`'s registers, which is fine
    // since nothing here reshapes registers the way `compile`'s `WHERE`
    // `Opcode::Filter` does.
    let mut ctx = Ctx {
        next_reg: needed.len(),
        column_regs: needed
            .iter()
            .enumerate()
            .map(|(i, n)| (n.clone(), i))
            .collect(),
        program,
    };
    let filtered_arg: Vec<Option<usize>> = items
        .iter()
        .map(|item| match item {
            Item::Window(spec) => {
                let base = spec.arg.as_ref().map(|name| ctx.load_column(name));
                mask_filtered_source(&mut ctx, base, spec.filter.as_ref())
            }
            _ => None,
        })
        .collect();
    // WHERE before the windows (#679): the Filter's selection applies to
    // every register loaded above, and `Window` resolves it first.
    if let Some(where_clause) = &select.where_clause {
        let predicate = compile_expr(where_clause, &mut ctx);
        ctx.push_commented(
            Opcode::Filter { predicate },
            format!("WHERE {}", expr_to_string(where_clause)),
        );
    }
    let mut program = ctx.program;
    let mut next_reg = ctx.next_reg;
    let mut null_reg: Option<usize> = None;
    let mut emit_regs = Vec::with_capacity(items.len());
    // One register per output item, in `SELECT`-list order.
    for (item, filtered_arg) in items.iter().zip(&filtered_arg) {
        match item {
            Item::Column(name) => emit_regs.push(column_reg(name)?),
            Item::Window(spec) => {
                let dst = next_reg;
                next_reg = next_reg.saturating_add(1);
                program.push(Instruction::with_comment(
                    Opcode::Window {
                        func: map_window_func(spec.func),
                        // Already resolved (and `FILTER`-masked, if any)
                        // above -- same register `column_reg(arg)` would
                        // give when there's no filter, since `needed`
                        // seeded `ctx`'s `column_regs`.
                        arg: *filtered_arg,
                        offset: spec.offset,
                        partition_by: spec
                            .partition_by
                            .iter()
                            .map(|p| column_reg(p))
                            .collect::<Result<Vec<_>>>()?
                            .into(),
                        order_by: spec
                            .order_by
                            .iter()
                            .map(|(o, desc)| Ok((column_reg(o)?, *desc)))
                            .collect::<Result<Vec<_>>>()?
                            .into(),
                        dst,
                    },
                    format!("r{dst} = {}", window_detail(spec)),
                ));
                emit_regs.push(dst);
            }
            Item::Agg(..) | Item::Star | Item::Expr(_) | Item::AggExpr(_) => {
                let reg = *null_reg.get_or_insert_with(|| {
                    let reg = next_reg;
                    next_reg = next_reg.saturating_add(1);
                    program.push(Instruction::new(Opcode::LoadConst {
                        reg,
                        value: Value::Null,
                    }));
                    reg
                });
                emit_regs.push(reg);
            }
        }
    }

    program.push(Instruction::with_comment(
        Opcode::Emit {
            registers: emit_regs.into(),
        },
        format!("SELECT {}", output_column_names(select).join(", ")),
    ));

    let sort = order_sort(select, None)?;
    let limit = select_limit(select);
    program.push(Instruction::with_comment(
        Opcode::Combine {
            agg_parts: Vec::new().into(),
            num_group_keys: 0,
            distinct: matches!(select.distinct, Some(Distinctness::Distinct)),
            output_order: Vec::new().into(),
            project: Vec::new().into(),
        },
        "concatenate segments".to_string(),
    ));
    if let Some((sort, comment)) = sort {
        program.push(Instruction::with_comment(sort, comment));
    }
    if let Some(n) = limit {
        program.push(Instruction::with_comment(
            Opcode::Limit {
                n,
                offset: select_offset(select),
            },
            format!("LIMIT {n}"),
        ));
    }

    Ok(Program::new(program))
}

/// `crate::codegen::batch::WindowFunc` and `crate::vm::batch::WindowFunc`
/// are separate types (same variants) so that the planner's own vocabulary
/// doesn't depend on the VM's execution-operand enum -- convert at the
/// point the planner hands a spec to the VM.
fn map_window_func(func: WindowFunc) -> crate::vm::batch::WindowFunc {
    use crate::vm::batch::WindowFunc as Vm;
    match func {
        WindowFunc::RowNumber => Vm::RowNumber,
        WindowFunc::Rank => Vm::Rank,
        WindowFunc::DenseRank => Vm::DenseRank,
        WindowFunc::Lag => Vm::Lag,
        WindowFunc::Lead => Vm::Lead,
        WindowFunc::FirstValue => Vm::FirstValue,
        WindowFunc::LastValue => Vm::LastValue,
        WindowFunc::Sum => Vm::Sum,
        WindowFunc::Avg => Vm::Avg,
        WindowFunc::Count => Vm::Count,
    }
}

/// `Opcode::Combine`'s `output_order` for a grouped `SELECT` (#585):
/// merged rows are keys-first, then one column per aggregate item in
/// select order, so a key item takes the next key position and an
/// aggregate item the next position after the keys. Only used when the
/// non-aggregate items are exactly the `GROUP BY` keys in `GROUP BY`
/// order; any other select list goes through `Combine`'s projection
/// ([`needs_projection`], #623). Empty when the order is the identity
/// (keys listed first), when there is no `GROUP BY`, or for any item shape
/// this mapping does not cover, which keeps the merged order.
fn grouped_output_order(items: &[Item], num_group_keys: usize) -> Vec<usize> {
    // #633: a `COUNT(DISTINCT x)` is merged right after the keys, in the
    // hidden key's slot, ahead of every other aggregate.
    let has_distinct = items.iter().any(|i| matches!(i, Item::Agg(_, _, _, true)));
    if num_group_keys == 0 && !has_distinct {
        return Vec::new();
    }
    let mut next_key = 0usize;
    let mut distinct_slot = num_group_keys;
    let mut next_agg = num_group_keys.saturating_add(usize::from(has_distinct));
    let mut order = Vec::with_capacity(items.len());
    for item in items {
        let slot = match item {
            Item::Column(_) | Item::Expr(_) => &mut next_key,
            Item::Agg(_, _, _, true) => &mut distinct_slot,
            Item::Agg(..) | Item::AggExpr(_) => &mut next_agg,
            Item::Star | Item::Window(_) => return Vec::new(),
        };
        order.push(*slot);
        *slot = slot.saturating_add(1);
    }
    let identity = order.iter().enumerate().all(|(i, &c)| i == c);
    if next_key != num_group_keys || identity {
        return Vec::new();
    }
    order
}

/// Derive each `SELECT`-list item's output column header (e.g. `SUM(amount)`,
/// `ROW_NUMBER()`) -- shared by the interpreter and the AOT emitter, which
/// both need the same naming for a compiled query's results.
pub fn output_column_names(select: &Select) -> Vec<String> {
    select.columns.iter().map(select_item_label).collect()
}

// ---------------------------------------------------------------------
// EXPLAIN
// ---------------------------------------------------------------------

/// One node in an [`explain`] plan tree: `parent == id` marks the root.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanNode {
    /// This node's identifier, unique within the plan.
    pub id: u32,
    /// The parent node's `id`; equal to `id` for the root.
    pub parent: u32,
    /// Human-readable description of the plan step.
    pub detail: String,
}

/// What `EXPLAIN`'s `SCAN` node reports about a table -- the only thing
/// the planner needs from storage, supplied by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableStats {
    /// Number of row groups (segments) in the table.
    pub row_groups: usize,
    /// Total row count of the table.
    pub rows: i64,
    /// A cross-mode join's source label for this table -- its execution
    /// mode and file, e.g. `"sqlite hosts.sqlite"` or `"stream app.log"`
    /// (#315, ADR-0019). `None` for a single-mode engine's own tables,
    /// where every `SCAN` in the plan is already understood to be the
    /// same file.
    pub source: Option<String>,
}

struct PlanBuilder {
    nodes: Vec<PlanNode>,
    next_id: u32,
}

impl PlanBuilder {
    fn new(root_detail: impl Into<String>) -> Self {
        PlanBuilder {
            nodes: vec![PlanNode {
                id: 0,
                parent: 0,
                detail: root_detail.into(),
            }],
            next_id: 1,
        }
    }

    fn push(&mut self, parent: u32, detail: impl Into<String>) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.nodes.push(PlanNode {
            id,
            parent,
            detail: detail.into(),
        });
        id
    }

    fn finish(self) -> Vec<PlanNode> {
        self.nodes
    }
}

/// Build a human-readable execution plan for `select` without running it
/// (#99). Mirrors the executor's dispatch (semi-join, join, windowed, or
/// plain single-table) over the same planning decisions [`compile`] makes;
/// `stats` supplies each referenced table's `SCAN` detail.
pub fn explain(select: &Select, stats: impl Fn(&str) -> TableStats) -> Result<Vec<PlanNode>> {
    let mut b = PlanBuilder::new("QUERY PLAN");

    let items = classify_items(select)?;
    let has_window = items.iter().any(|c| matches!(c, Item::Window(_)));
    let is_semi_join = matches!(
        &select.where_clause,
        Some(AstExpr {
            kind: ExprKind::InSubquery { .. },
            ..
        })
    );
    let from = select.from.as_ref();
    // No FROM is a legitimate (table-less) SELECT: empty name, no joins.
    let from_name = from.map(table_name).transpose()?.unwrap_or_default();
    let joins = from.map(extract_joins).transpose()?.unwrap_or_default();
    let join = joins.first();

    // Semi-joins compile with `where_clause` stripped, mirroring
    // `compile_semi_join` (the `IN` subquery isn't a VM predicate).
    let program = if has_window {
        None
    } else if is_semi_join {
        let mut stripped = select.clone();
        stripped.where_clause = None;
        Some(compile(&stripped)?)
    } else {
        Some(compile(select)?)
    };
    let columns_to_load = program.as_ref().map(Program::columns_to_load);

    // A join's own EXPLAIN needs the driving and lookup sides labelled separately.
    let mut main_cols: Vec<String> = match (&columns_to_load, join) {
        (Some(cols), Some(_)) => cols
            .iter()
            .filter(|n| split_qualified(n).0.is_none_or(|t| t == from_name))
            .cloned()
            .collect(),
        (Some(cols), None) => cols.clone(),
        (None, _) => referenced_columns(select)?,
    };
    if let Some(j) = join {
        push_unique(&mut main_cols, j.left_col.clone());
    }
    if let Some(AstExpr {
        kind: ExprKind::InSubquery { expr, .. },
        ..
    }) = &select.where_clause
    {
        if let Some(col_name) = expr_column_name(expr) {
            push_unique(&mut main_cols, col_name);
        }
    }
    let scan = b.push(0, scan_detail(from_name, stats(from_name)));
    if !main_cols.is_empty() {
        b.push(scan, format!("LOAD COLUMNS: {}", main_cols.join(", ")));
    }

    if is_semi_join {
        if let Some(AstExpr {
            kind: ExprKind::InSubquery { expr, subquery, .. },
            ..
        }) = &select.where_clause
        {
            let sub_from = subquery
                .from
                .as_ref()
                .map(table_name)
                .transpose()?
                .unwrap_or_default();
            let sub_scan = b.push(0, scan_detail(sub_from, stats(sub_from)));
            let sub_cols = referenced_columns(subquery)?;
            if !sub_cols.is_empty() {
                b.push(sub_scan, format!("LOAD COLUMNS: {}", sub_cols.join(", ")));
            }
            let sub_select: Vec<String> = subquery.columns.iter().map(select_item_label).collect();
            b.push(
                0,
                format!(
                    "SEMI JOIN: {} IN (SELECT {} FROM {})",
                    expr_to_string(expr),
                    sub_select.join(", "),
                    sub_from
                ),
            );
        }
    } else if let Some(join) = join {
        let mut right_cols: Vec<String> = columns_to_load
            .as_ref()
            .map(|cols| {
                cols.iter()
                    .filter(|n| split_qualified(n).0 == Some(join.table.as_str()))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        push_unique(&mut right_cols, join.right_col.clone());
        let right_scan = b.push(0, scan_detail(&join.table, stats(&join.table)));
        if !right_cols.is_empty() {
            b.push(
                right_scan,
                format!("LOAD COLUMNS: {}", right_cols.join(", ")),
            );
        }
        let kind = match join.op {
            JoinOp::Inner => "HASH JOIN",
            JoinOp::Left => "LEFT HASH JOIN",
            JoinOp::Right => "RIGHT HASH JOIN",
            JoinOp::Full => "FULL HASH JOIN",
            JoinOp::Cross => "CROSS JOIN",
        };
        b.push(0, format!("{kind}: {} = {}", join.left_col, join.right_col));
    }

    if has_window {
        for item in &items {
            if let Item::Window(spec) = item {
                b.push(0, format!("WINDOW: {}", window_detail(spec)));
            }
        }
    } else if let Some(program) = &program {
        if program
            .opcodes()
            .any(|op| matches!(op, Opcode::Filter { .. }))
        {
            if let Some(where_clause) = &select.where_clause {
                b.push(0, format!("FILTER: {}", expr_to_string(where_clause)));
            }
        }
        let group_by: Vec<String> = select
            .group_by
            .iter()
            .filter_map(expr_column_name)
            .collect();
        if !group_by.is_empty() {
            let group_node = b.push(0, format!("GROUP BY: {}", group_by.join(", ")));
            for item in &select.columns {
                if matches!(classify_item(item), Ok(Item::Agg(..))) {
                    b.push(
                        group_node,
                        format!("AGGREGATE: {}", select_item_label(item)),
                    );
                }
            }
        } else {
            for item in &select.columns {
                if matches!(classify_item(item), Ok(Item::Agg(..))) {
                    b.push(0, format!("AGGREGATE: {}", select_item_label(item)));
                }
            }
        }
        // DISTINCT runs as a post-Finalize dedup pass, after GROUP BY's
        // hash-aggregate merge and before ORDER BY/LIMIT (see
        // `compile`'s `distinct` handling) -- the plan reflects that order.
        if matches!(select.distinct, Some(Distinctness::Distinct)) {
            b.push(0, "DISTINCT".to_string());
        }
    }

    let order_by = select_order_by(select);
    if !order_by.is_empty() {
        b.push(0, format!("ORDER BY: {}", order_by_text(&order_by)));
    }
    if let Some(limit) = select_limit(select) {
        b.push(0, format!("LIMIT: {limit}"));
    }

    let emit_labels: Vec<String> = select.columns.iter().map(select_item_label).collect();
    b.push(0, format!("EMIT: {}", emit_labels.join(", ")));

    Ok(b.finish())
}

// ---------------------------------------------------------------------
// EXPLAIN (bare form -- opcode listing)
// ---------------------------------------------------------------------

/// One row of a bare `EXPLAIN`'s opcode listing (#55): `addr | opcode |
/// operands | comment`, mirroring sqlite-rs's own bare-`EXPLAIN` table as
/// far as the batch executor's typed operands allow (ADR 0007).
#[derive(Debug, Clone, PartialEq)]
pub struct OpcodeRow {
    /// Instruction address within its program.
    pub addr: usize,
    /// Opcode name.
    pub opcode: &'static str,
    /// Rendered operands.
    pub operands: String,
    /// Explanatory comment for the instruction.
    pub comment: String,
    /// Whether this row is the [`Opcode::Combine`] barrier: the boundary
    /// between the parallel per-segment phase and the sequential
    /// cross-segment merge phase (ADR 0007, db-core#48).
    pub is_finalize: bool,
}

/// One named program in a bare `EXPLAIN` listing. A flat query has a
/// single section; a join has `build`/`probe`/`body` ([`JoinProgram`]); a
/// semi-join has a single `body` section (its subquery isn't opcode-driven
/// -- see [`compile_semi_join`]).
#[derive(Debug, Clone, PartialEq)]
pub struct OpcodeSection {
    /// Section name (e.g. `build`, `probe`, `body`).
    pub label: String,
    /// The section's instructions, in address order.
    pub rows: Vec<OpcodeRow>,
}

pub(crate) fn render_program(program: &Program) -> Vec<OpcodeRow> {
    program
        .instructions
        .iter()
        .enumerate()
        .map(|(addr, instr)| OpcodeRow {
            addr,
            opcode: instr.opcode.name(),
            operands: render_operands(&instr.opcode),
            comment: instr.comment.clone().unwrap_or_default(),
            is_finalize: matches!(instr.opcode, Opcode::Combine { .. }),
        })
        .collect()
}

fn render_agg_pair(func: AggFunc, src: Option<usize>) -> String {
    match src {
        Some(s) => format!("{func:?}({s})"),
        None => format!("{func:?}"),
    }
}

/// Human-readable operands for one [`Opcode`], named-field style (not a
/// `Debug` dump) -- e.g. `reg=0 column=product`, `group_by=[0]
/// aggs=[Sum(1)] agg_dst=[4]`.
fn render_operands(op: &Opcode) -> String {
    match op {
        Opcode::LoadColumn { reg, column } => format!("reg={reg} column={column}"),
        Opcode::LoadConst { reg, value } => format!("reg={reg} value={value}"),
        Opcode::Map { dst, op, a, b } => format!("dst={dst} op={op:?} a={a} b={b}"),
        Opcode::Filter { predicate } => format!("predicate=r{predicate}"),
        Opcode::Reduce { func, src, dst } => {
            format!("dst={dst} {}", render_agg_pair(*func, *src))
        }
        Opcode::GroupReduce {
            group_by,
            aggs,
            agg_dst,
        } => {
            let aggs: Vec<String> = aggs
                .iter()
                .map(|(func, src)| render_agg_pair(*func, *src))
                .collect();
            format!(
                "group_by={group_by:?} aggs=[{}] agg_dst={agg_dst:?}",
                aggs.join(", ")
            )
        }
        Opcode::HashBuild {
            key_cols,
            payload_cols,
            table,
        } => format!("key_cols={key_cols:?} payload_cols={payload_cols:?} table={table}"),
        Opcode::HashProbe {
            key_cols,
            table,
            payload_dst,
            kind,
        } => format!("key_cols={key_cols:?} table={table} payload_dst={payload_dst:?} kind={kind:?}"),
        Opcode::HashProbeGroupReduce {
            key_cols,
            table,
            kind,
            group_by,
            aggs,
            agg_dst,
        } => {
            let aggs: Vec<String> = aggs
                .iter()
                .map(|(func, src)| match src {
                    Some(s) => format!("{func:?}({s:?})"),
                    None => format!("{func:?}"),
                })
                .collect();
            format!(
                "key_cols={key_cols:?} table={table} kind={kind:?} group_by={group_by:?} aggs=[{}] agg_dst={agg_dst:?}",
                aggs.join(", ")
            )
        }
        Opcode::Window {
            func,
            arg,
            offset,
            partition_by,
            order_by,
            dst,
        } => format!(
            "dst={dst} func={func:?} arg={arg:?} offset={offset:?} partition_by={partition_by:?} order_by={order_by:?}"
        ),
        Opcode::Scan => String::new(),
        Opcode::ScanSource(source) => render_scan_source(source),
        Opcode::Emit { registers } => format!("registers={registers:?}"),
        Opcode::NextSegment { loop_start } => format!("loop_start={loop_start}"),
        Opcode::Halt => String::new(),
        Opcode::Combine {
            agg_parts,
            num_group_keys,
            distinct,
            output_order,
            project,
        } => {
            let mut operands =
                format!("agg_parts={agg_parts:?} num_group_keys={num_group_keys} distinct={distinct}");
            if !output_order.is_empty() {
                operands = format!("{operands} output_order={output_order:?}");
            }
            if !project.is_empty() {
                let steps: Vec<String> = project
                    .iter()
                    .map(|op| format!("{} {}", op.name(), render_operands(op)))
                    .collect();
                operands = format!("{operands} project=[{}]", steps.join("; "));
            }
            operands
        }
        Opcode::Sort { keys, hidden } => format!("keys={keys:?} hidden={hidden}"),
        Opcode::Limit { n, offset } => format!("n={n} offset={offset}"),
        Opcode::Call { dst, name, args } => format!("dst={dst} name={name} args={args:?}"),
        // The item count, not the items: a list of a hundred ids would
        // drown the plan.
        Opcode::InList {
            dst,
            src,
            list,
            negated,
        } => format!("dst={dst} src={src} items={} negated={negated}", list.len()),
    }
}

/// Human-readable operands for one [`crate::vm::batch::ScanSource`] --
/// names the lane (row/stream/in-memory) `Opcode::ScanSource` reads its
/// build side from, per ADR 0024's requirement that cross-mode `EXPLAIN`
/// output keep each source's origin visible.
fn render_scan_source(source: &crate::vm::batch::ScanSource) -> String {
    use crate::vm::batch::ScanSource;
    match source {
        ScanSource::RowTable { table, columns } => {
            format!("row table={table} columns={columns:?}")
        }
        #[cfg(feature = "vm-stream")]
        ScanSource::Stream {
            handle,
            columns,
            scope,
        } => format!("stream handle={handle} columns={columns:?} scope={scope:?}"),
        #[cfg(not(feature = "vm-stream"))]
        ScanSource::Stream { handle, columns } => {
            format!("stream handle={handle} columns={columns:?}")
        }
        ScanSource::InMemory(batch) => format!("in-memory rows={}", batch.num_rows),
    }
}

/// Build a bare `EXPLAIN`'s opcode listing for `select` (#55): the compiled
/// [`Program`]'s instructions, one section per phase the executor actually
/// runs -- mirrors [`explain`]'s shape dispatch (semi-join, join, windowed,
/// or plain single-table) but over the real compiled opcodes instead of a
/// hand-built plan tree. One [`OpcodeSection`] per phase, not one per
/// `Opcode`, since that's the granularity a reader debugging a plan cares
/// about.
pub fn explain_opcodes(select: &Select) -> Result<Vec<OpcodeSection>> {
    let items = classify_items(select).unwrap_or_default();
    let has_window = items.iter().any(|c| matches!(c, Item::Window(_)));

    if matches!(
        &select.where_clause,
        Some(AstExpr {
            kind: ExprKind::InSubquery { .. },
            ..
        })
    ) {
        let semi = compile_semi_join(select)?;
        let sub_from = semi
            .subquery
            .from
            .as_ref()
            .map(table_name)
            .transpose()?
            .unwrap_or_default();
        Ok(vec![OpcodeSection {
            label: format!(
                "SEMI JOIN body ({} IN (SELECT ... FROM {}))",
                semi.key_column, sub_from
            ),
            rows: render_program(&semi.body),
        }])
    } else if select.from.as_ref().is_some_and(|f| !f.joins.is_empty()) {
        let join = compile_join(select, BuildSourceKind::InMemory)?;
        let table = select
            .from
            .as_ref()
            .and_then(|f| f.joins.first())
            .and_then(|j| j.table.name())
            .unwrap_or_default()
            .to_string();
        Ok(vec![
            OpcodeSection {
                label: format!("JOIN build ({table})"),
                rows: render_program(&join.build),
            },
            OpcodeSection {
                label: "JOIN probe".to_string(),
                rows: render_program(&join.probe),
            },
            OpcodeSection {
                label: "JOIN body".to_string(),
                rows: render_program(&join.body),
            },
        ])
    } else if has_window {
        Ok(vec![OpcodeSection {
            label: "body".to_string(),
            rows: render_program(&compile_window(select)?),
        }])
    } else {
        Ok(vec![OpcodeSection {
            label: "body".to_string(),
            rows: render_program(&compile(select)?),
        }])
    }
}

fn scan_detail(table: &str, stats: TableStats) -> String {
    let groups = stats.row_groups;
    let label = stats
        .source
        .as_deref()
        .map_or(String::new(), |src| format!(" [{src}]"));
    // A SQLite table has no row groups (#789): the count alone.
    if stats
        .source
        .as_deref()
        .is_some_and(|src| src.starts_with("sqlite "))
    {
        return format!("SCAN {table} (~{} rows){label}", stats.rows);
    }
    format!(
        "SCAN {table} ({groups} row group{}, ~{} rows){label}",
        if groups == 1 { "" } else { "s" },
        stats.rows
    )
}

fn agg_func_name(func: AggFunc) -> &'static str {
    func.name()
}

fn window_func_name(func: WindowFunc) -> &'static str {
    match func {
        WindowFunc::RowNumber => "ROW_NUMBER",
        WindowFunc::Rank => "RANK",
        WindowFunc::DenseRank => "DENSE_RANK",
        WindowFunc::Lag => "LAG",
        WindowFunc::Lead => "LEAD",
        WindowFunc::FirstValue => "FIRST_VALUE",
        WindowFunc::LastValue => "LAST_VALUE",
        WindowFunc::Sum => "SUM",
        WindowFunc::Avg => "AVG",
        WindowFunc::Count => "COUNT",
    }
}

/// Output column label for one `SELECT` item, e.g. `amount`, `SUM(amount)`,
/// `COUNT(*)`, or `ROW_NUMBER()` -- or, when the item carries an explicit
/// `AS <alias>` (#307), that alias verbatim.
fn select_item_label(item: &ResultColumn) -> String {
    if let ResultColumn::Expr {
        alias: Some(alias), ..
    } = item
    {
        return alias.clone();
    }
    match classify_item(item) {
        Ok(Item::Column(name)) => name,
        Ok(Item::Star) => "*".to_string(),
        Ok(Item::Agg(func, Some(AggArg::Column(col)), _, true)) => {
            format!("{}(DISTINCT {col})", agg_func_name(func))
        }
        Ok(Item::Agg(func, Some(AggArg::Expr(expr)), _, true)) => {
            format!(
                "{}(DISTINCT {})",
                agg_func_name(func),
                expr_to_string(&expr)
            )
        }
        Ok(Item::Agg(func, arg, _, _)) => match arg {
            Some(AggArg::Column(col)) => format!("{}({col})", agg_func_name(func)),
            Some(AggArg::Expr(expr)) => {
                format!("{}({})", agg_func_name(func), expr_to_string(&expr))
            }
            None => format!("{}(*)", agg_func_name(func)),
        },
        Ok(Item::Window(spec)) => format!("{}()", window_func_name(spec.func)),
        Ok(Item::Expr(expr) | Item::AggExpr(expr)) => expr_to_string(&expr),
        Err(_) => String::new(),
    }
}

fn window_detail(spec: &WindowSpec) -> String {
    let mut detail = format!(
        "{}({})",
        window_func_name(spec.func),
        spec.arg.as_deref().unwrap_or("")
    );
    let mut over = Vec::new();
    if !spec.partition_by.is_empty() {
        over.push(format!("PARTITION BY {}", spec.partition_by.join(", ")));
    }
    if !spec.order_by.is_empty() {
        let cols: Vec<String> = spec
            .order_by
            .iter()
            .map(|(col, desc)| {
                // Render each ORDER BY term's direction.
                if *desc {
                    format!("{col} DESC")
                } else {
                    col.clone()
                }
            })
            .collect();
        over.push(format!("ORDER BY {}", cols.join(", ")));
    }
    if !over.is_empty() {
        detail.push_str(" OVER (");
        detail.push_str(&over.join(" "));
        detail.push(')');
    }
    detail
}

fn literal_to_string(lit: &AstLiteral) -> String {
    match lit {
        AstLiteral::Integer(v) => v.to_string(),
        AstLiteral::Float(v) => v.to_string(),
        AstLiteral::Str(v) => format!("'{v}'"),
        AstLiteral::Blob(_) => "x'...'".to_string(),
        AstLiteral::Null => "NULL".to_string(),
        AstLiteral::True => "TRUE".to_string(),
        AstLiteral::False => "FALSE".to_string(),
    }
}

fn bin_op_str(op: AstBinOp) -> &'static str {
    match op {
        AstBinOp::Add => "+",
        AstBinOp::Sub => "-",
        AstBinOp::Mul => "*",
        AstBinOp::Div => "/",
        AstBinOp::Eq => "=",
        AstBinOp::Ne => "!=",
        AstBinOp::Lt => "<",
        AstBinOp::Le => "<=",
        AstBinOp::Gt => ">",
        AstBinOp::Ge => ">=",
        AstBinOp::And => "AND",
        AstBinOp::Or => "OR",
        AstBinOp::Concat => "||",
        AstBinOp::BitAnd => "&",
        AstBinOp::BitOr => "|",
        AstBinOp::Shl => "<<",
        AstBinOp::Shr => ">>",
        AstBinOp::Mod => "%",
    }
}

/// Renders an `Expr` back to SQL-ish text for plan details, e.g.
/// `amount > 100`.
fn expr_to_string(expr: &AstExpr) -> String {
    match &expr.kind {
        // A `Column` always carries a name; `expr_column_name` is `None`
        // only for non-column kinds, which this arm excludes. Plan text.
        ExprKind::Column { .. } => expr_column_name(expr).unwrap_or_default(),
        ExprKind::Literal(lit) => literal_to_string(lit),
        ExprKind::Paren(inner) => expr_to_string(inner),
        ExprKind::Binary { op, lhs, rhs } => {
            format!(
                "{} {} {}",
                expr_to_string(lhs),
                bin_op_str(*op),
                expr_to_string(rhs)
            )
        }
        ExprKind::InSubquery { expr, subquery, .. } => {
            // Plan text only: the real compile reports a missing alias as
            // `PlanError::Internal`; here it just renders as "".
            let from = subquery
                .from
                .as_ref()
                .and_then(|f| table_name(f).ok())
                .unwrap_or_default();
            format!("{} IN (SELECT ... FROM {from})", expr_to_string(expr))
        }
        ExprKind::Exists { subquery, negated } => {
            // Plan text only: the real compile reports a missing alias as
            // `PlanError::Internal`; here it just renders as "".
            let from = subquery
                .from
                .as_ref()
                .and_then(|f| table_name(f).ok())
                .unwrap_or_default();
            format!(
                "{}EXISTS (SELECT ... FROM {from})",
                if *negated { "NOT " } else { "" }
            )
        }
        ExprKind::Unary {
            op: crate::parser::ast::UnaryOp::Not,
            expr: inner,
        } => format!("NOT {}", expr_to_string(inner)),
        ExprKind::Unary {
            op: crate::parser::ast::UnaryOp::Minus,
            expr: inner,
        } => format!("-{}", expr_to_string(inner)),
        ExprKind::Unary { expr: inner, .. } => expr_to_string(inner),
        ExprKind::IsNull { expr, negated } => {
            format!(
                "{} IS {}NULL",
                expr_to_string(expr),
                if *negated { "NOT " } else { "" }
            )
        }
        ExprKind::Is { lhs, rhs, negated }
            if matches!(rhs.kind, ExprKind::Literal(AstLiteral::Null)) =>
        {
            format!(
                "{} IS {}NULL",
                expr_to_string(lhs),
                if *negated { "NOT " } else { "" }
            )
        }
        _ => String::new(),
    }
}

fn push_unique(out: &mut Vec<String>, name: String) {
    if !out.contains(&name) {
        out.push(name);
    }
}

fn collect_expr_columns(expr: &AstExpr, out: &mut Vec<String>) {
    match &expr.kind {
        ExprKind::Column { .. } => {
            if let Some(name) = expr_column_name(expr) {
                push_unique(out, name);
            }
        }
        ExprKind::Literal(_) => {}
        ExprKind::Paren(inner) => collect_expr_columns(inner, out),
        ExprKind::Binary { lhs, rhs, .. } => {
            collect_expr_columns(lhs, out);
            collect_expr_columns(rhs, out);
        }
        ExprKind::InSubquery { expr, .. } => collect_expr_columns(expr, out),
        ExprKind::Exists { .. } => {}
        ExprKind::Unary { expr: inner, .. } => collect_expr_columns(inner, out),
        ExprKind::IsNull { expr, .. } => collect_expr_columns(expr, out),
        ExprKind::Is { lhs, .. } => collect_expr_columns(lhs, out),
        ExprKind::Like { expr, pattern, .. } => {
            collect_expr_columns(expr, out);
            collect_expr_columns(pattern, out);
        }
        // A scalar call's (#307) column references are its arguments' --
        // needed so a bare (non-`GROUP BY`) `SELECT json_extract(msg, ...)`
        // pre-loads `msg` before `Filter`, same as any other expression.
        ExprKind::FunctionCall {
            args: FunctionArgs::List(list),
            ..
        } => {
            for arg in list {
                collect_expr_columns(arg, out);
            }
        }
        _ => {}
    }
}

/// Every column name `select` references, in first-seen order (used for the
/// `EXPLAIN` `LOAD COLUMNS` detail on paths that don't go through
/// [`compile`], namely windowed queries).
fn referenced_columns(select: &Select) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for name in select.group_by.iter().filter_map(expr_column_name) {
        push_unique(&mut out, name);
    }
    let items = classify_items(select)?;
    for item in &items {
        match item {
            Item::Column(name) => push_unique(&mut out, name.clone()),
            Item::Star => {}
            Item::Agg(_, arg, filter, _) => {
                match arg {
                    Some(AggArg::Column(name)) => push_unique(&mut out, name.clone()),
                    Some(AggArg::Expr(expr)) => collect_expr_columns(expr, &mut out),
                    None => {}
                }
                if let Some(f) = filter {
                    let mut cols = Vec::new();
                    collect_expr_columns(f, &mut cols);
                    for name in cols {
                        push_unique(&mut out, name);
                    }
                }
            }
            Item::Window(spec) => {
                if let Some(arg) = &spec.arg {
                    push_unique(&mut out, arg.clone());
                }
                for name in &spec.partition_by {
                    push_unique(&mut out, name.clone());
                }
                for (name, _) in &spec.order_by {
                    push_unique(&mut out, name.clone());
                }
                if let Some(f) = &spec.filter {
                    collect_expr_columns(f, &mut out);
                }
            }
            Item::Expr(expr) | Item::AggExpr(expr) => collect_expr_columns(expr, &mut out),
        }
    }
    if let Some(where_clause) = &select.where_clause {
        collect_expr_columns(where_clause, &mut out);
    }
    if let Some(from) = &select.from {
        for join in extract_joins(from)? {
            push_unique(&mut out, join.left_col.clone());
            push_unique(&mut out, join.right_col.clone());
        }
    }
    for (column, _) in select_order_by(select) {
        // #131: an `ORDER BY` referencing a SELECT-list aggregate carries
        // that aggregate's rendered label (e.g. `COUNT(x)`), not a real
        // column name -- its underlying column, if any, is already
        // covered above via that item's own aggregate item, so only push
        // here when the label isn't itself a `SELECT`-list item (i.e.
        // it's a genuine bare-column reference).
        let is_select_item_label = select
            .columns
            .iter()
            .any(|item| select_item_label(item) == column);
        if !is_select_item_label {
            push_unique(&mut out, column);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser as sql;
    use crate::parser::ast::JoinOp;

    #[test]
    fn plan_error_display_covers_every_variant() {
        // `NoJoinClause` and `Internal` each already have their own
        // dedicated test elsewhere; the rest of `PlanError`'s Display
        // arms are otherwise untested.
        assert_eq!(
            PlanError::UnknownColumn("x".into()).to_string(),
            "unknown column: x"
        );
        assert_eq!(
            PlanError::UnsupportedSemiJoin("shape".into()).to_string(),
            "unsupported semi-join: shape"
        );
        assert_eq!(
            PlanError::UnsupportedJoinKind(JoinOp::Cross).to_string(),
            "join kind Cross is not yet executable (only Inner/Left are implemented)"
        );
        assert_eq!(
            PlanError::StarWithAggregation.to_string(),
            "SELECT * cannot be combined with GROUP BY, an aggregate, or a window function"
        );
        assert_eq!(
            PlanError::UnsupportedSelectItem("thing".into()).to_string(),
            "unsupported SELECT item: thing"
        );
        assert_eq!(
            PlanError::ScopeClauseUnsupported.to_string(),
            "SINCE/UNTIL is only available through the stream engine"
        );
    }

    #[test]
    fn window_func_from_name_covers_every_recognized_function_and_rejects_the_rest() {
        // Only `ROW_NUMBER` is exercised end-to-end (through a real
        // `OVER` clause) anywhere else in this crate's test suite; the
        // rest of `from_name`'s match arms are otherwise never reached.
        let cases: &[(&str, WindowFunc)] = &[
            ("ROW_NUMBER", WindowFunc::RowNumber),
            ("row_number", WindowFunc::RowNumber),
            ("RANK", WindowFunc::Rank),
            ("DENSE_RANK", WindowFunc::DenseRank),
            ("LAG", WindowFunc::Lag),
            ("LEAD", WindowFunc::Lead),
            ("FIRST_VALUE", WindowFunc::FirstValue),
            ("LAST_VALUE", WindowFunc::LastValue),
            ("SUM", WindowFunc::Sum),
            ("AVG", WindowFunc::Avg),
            ("COUNT", WindowFunc::Count),
        ];
        for &(name, expected) in cases {
            assert_eq!(WindowFunc::from_name(name), Some(expected), "{name}");
        }
        assert_eq!(WindowFunc::from_name("NOT_A_WINDOW_FUNC"), None);
    }

    /// db-core#231: dispatch only routes joined queries to `compile_join`;
    /// a caller that doesn't gets a typed error, not a panic.
    #[test]
    fn compile_join_without_a_join_clause_is_a_plan_error() {
        let query = sql::parse("SELECT a FROM t").unwrap();
        assert_eq!(
            compile_join(&query, BuildSourceKind::InMemory).err(),
            Some(PlanError::NoJoinClause)
        );
    }
    use crate::vm::engine::bounded_scan_limit;

    #[test]
    fn bounded_scan_limit_accepts_bare_limit() {
        let program = compile(&sql::parse("SELECT id FROM t LIMIT 10").unwrap()).unwrap();
        assert_eq!(bounded_scan_limit(&program), Some(10));
    }

    #[test]
    fn bounded_scan_limit_rejects_where_order_by_group_by_and_aggregates() {
        for q in [
            "SELECT id FROM t WHERE id > 1 LIMIT 10",
            "SELECT id FROM t ORDER BY id LIMIT 10",
            "SELECT id, SUM(amount) FROM t GROUP BY id LIMIT 10",
            "SELECT COUNT(*) FROM t LIMIT 10",
            "SELECT id FROM t",
        ] {
            let program = compile(&sql::parse(q).unwrap()).unwrap();
            assert_eq!(bounded_scan_limit(&program), None, "{q}");
        }
    }

    #[test]
    fn compile_where_and_group_by_builds_expected_program_shape() {
        let query =
            sql::parse("SELECT region, SUM(amount) FROM t WHERE amount > 10 GROUP BY region")
                .unwrap();
        let program = compile(&query).unwrap();
        let columns = program.columns_to_load();
        assert!(columns.contains(&"region".to_string()));
        assert!(columns.contains(&"amount".to_string()));
        assert!(matches!(
            program.opcodes().last(),
            Some(Opcode::Combine {
                num_group_keys: 1,
                ..
            })
        ));
        let (body, ..) = program.split_finalize();
        assert!(matches!(body.last(), Some(Opcode::Emit { .. })));
        assert!(body
            .iter()
            .any(|op| matches!(op, Opcode::GroupReduce { .. })));
        assert!(body.iter().any(|op| matches!(op, Opcode::Filter { .. })));
    }

    /// `(items, negated)` of each [`Opcode::InList`] the `WHERE` of `query`
    /// compiles to, as the column dialect lowers it, and how many `Map`s
    /// remain.
    fn in_lists(query: &str) -> (Vec<(Vec<Value>, bool)>, usize) {
        let program = compile(&crate::parser::column::parse(query).unwrap()).unwrap();
        let (body, ..) = program.split_finalize();
        let lists = body
            .iter()
            .filter_map(|op| match op {
                Opcode::InList { list, negated, .. } => Some((list.to_vec(), *negated)),
                _ => None,
            })
            .collect();
        let maps = body
            .iter()
            .filter(|op| matches!(op, Opcode::Map { .. }))
            .count();
        (lists, maps)
    }

    /// #737: a literal `IN` list is one set probe, not a comparison and an
    /// `OR` per item; inside a conjunction too.
    #[test]
    fn in_list_of_literals_compiles_to_one_in_list_probe() {
        let s = |x: &str| Value::Str(x.to_string().into());
        assert_eq!(
            in_lists("SELECT id FROM t WHERE name IN ('a', 'b', 'c')"),
            (vec![(vec![s("a"), s("b"), s("c")], false)], 0)
        );
        assert_eq!(
            in_lists("SELECT id FROM t WHERE id NOT IN (1, 2)"),
            (vec![(vec![Value::Int(1), Value::Int(2)], true)], 0)
        );
        // The comparison and the AND around it stay `Map`s.
        assert_eq!(
            in_lists("SELECT id FROM t WHERE ts >= 5 AND name IN ('a', 'b')"),
            (vec![(vec![s("a"), s("b")], false)], 2)
        );
        // Written out by hand, through parentheses.
        assert_eq!(
            in_lists("SELECT id FROM t WHERE (name = 'a' OR name = 'b') OR name = 'c'").0,
            vec![(vec![s("a"), s("b"), s("c")], false)]
        );
    }

    // codegen_batch_in_list_chain_1c58189f (`in_list_chain`):
    // `*op != cmp || column.as_ref().is_some_and(|c| *c != name)`
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_batch_in_list_chain_1c58189f__v1_same_op_same_column_is_a_list() {
        assert_eq!(
            in_lists("SELECT id FROM t WHERE a = 1 OR a = 2").0,
            vec![(vec![Value::Int(1), Value::Int(2)], false)]
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_batch_in_list_chain_1c58189f__v2_another_op_is_no_list() {
        assert!(in_lists("SELECT id FROM t WHERE a = 1 OR a < 2")
            .0
            .is_empty());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_batch_in_list_chain_1c58189f__v3_another_column_is_no_list() {
        assert!(in_lists("SELECT id FROM t WHERE a = 1 OR b = 2")
            .0
            .is_empty());
    }

    #[test]
    fn in_list_chain_that_is_not_one_column_against_literals_compiles_as_before() {
        for query in [
            // One item: a plain `=`, which keeps the dictionary fast path.
            "SELECT id FROM t WHERE name IN ('a')",
            // An expression item.
            "SELECT id FROM t WHERE id IN (1, 1 + 1)",
            // Two columns.
            "SELECT id FROM t WHERE a = 1 OR b = 2",
            // `<>` under OR, `=` under AND: no IN list.
            "SELECT id FROM t WHERE a <> 1 OR a <> 2",
            "SELECT id FROM t WHERE a = 1 AND a = 2",
            // A column on the right.
            "SELECT id FROM t WHERE a = b OR a = c",
        ] {
            let (lists, maps) = in_lists(query);
            assert!(lists.is_empty(), "{query}");
            assert!(maps > 0, "{query}");
        }
    }

    #[test]
    fn compile_projects_computed_select_list_expression_via_map_into_emit() {
        let query = sql::parse("SELECT x * 2 + 1, a || b FROM t").unwrap();
        let program = compile(&query).unwrap();
        let columns = program.columns_to_load();
        assert!(columns.contains(&"x".to_string()));
        assert!(columns.contains(&"a".to_string()));
        assert!(columns.contains(&"b".to_string()));
        let (body, ..) = program.split_finalize();
        assert!(body.iter().any(|op| matches!(
            op,
            Opcode::Map {
                op: MapOp::Mul | MapOp::Add | MapOp::Concat,
                ..
            }
        )));
        assert!(matches!(body.last(), Some(Opcode::Emit { .. })));
        assert_eq!(
            output_column_names(&query),
            vec!["x * 2 + 1".to_string(), "a || b".to_string()]
        );
    }

    #[test]
    fn compile_distinct_sets_finalize_flag_without_group_reduce() {
        let query = sql::parse("SELECT DISTINCT a, b FROM t").unwrap();
        let program = compile(&query).unwrap();
        assert!(matches!(
            program.opcodes().last(),
            Some(Opcode::Combine {
                distinct: true,
                num_group_keys: 0,
                ..
            })
        ));
        let (body, ..) = program.split_finalize();
        assert!(!body
            .iter()
            .any(|op| matches!(op, Opcode::GroupReduce { .. })));
    }

    #[test]
    fn compile_distinct_with_group_by_sets_both_finalize_flag_and_group_reduce() {
        let query = sql::parse("SELECT DISTINCT region, SUM(amount) FROM t GROUP BY region")
            .expect("DISTINCT + GROUP BY should parse (rewritten as a post-aggregate dedup)");
        let program = compile(&query).unwrap();
        assert!(matches!(
            program.opcodes().last(),
            Some(Opcode::Combine {
                distinct: true,
                num_group_keys: 1,
                ..
            })
        ));
        let (body, ..) = program.split_finalize();
        assert!(body
            .iter()
            .any(|op| matches!(op, Opcode::GroupReduce { .. })));
    }

    #[test]
    fn compile_encodes_order_by_and_limit_as_sort_and_limit_and_comments_instructions() {
        let query = sql::parse("SELECT id, val FROM t ORDER BY val DESC LIMIT 5").unwrap();
        let program = compile(&query).unwrap();
        let tail: Vec<&Instruction> = program.instructions.iter().rev().take(3).rev().collect();
        assert_eq!(
            tail[0].opcode,
            Opcode::Combine {
                agg_parts: Vec::new().into(),
                num_group_keys: 0,
                distinct: false,
                output_order: Vec::new().into(),
                project: Vec::new().into(),
            }
        );
        assert_eq!(tail[0].comment.as_deref(), Some("concatenate segments"));
        assert_eq!(
            tail[1].opcode,
            Opcode::Sort {
                keys: vec![(1, true)].into(),
                hidden: 0
            }
        );
        assert_eq!(tail[1].comment.as_deref(), Some("ORDER BY val DESC"));
        assert_eq!(tail[2].opcode, Opcode::Limit { n: 5, offset: 0 });
        assert_eq!(tail[2].comment.as_deref(), Some("LIMIT 5"));
        assert!(program.instructions.iter().all(|i| i.comment.is_some()));
    }

    #[test]
    fn compile_encodes_order_by_referencing_a_select_list_aggregate() {
        // #131: ORDER BY may reference a SELECT-list aggregate by the
        // same output position resolution as ORDER BY on a plain column.
        let query = sql::parse(
            "SELECT customer_id, COUNT(event_id), SUM(amount) FROM t \
             GROUP BY customer_id ORDER BY COUNT(event_id) DESC",
        )
        .unwrap();
        let program = compile(&query).unwrap();
        let tail: Vec<&Instruction> = program.instructions.iter().rev().take(2).rev().collect();
        assert_eq!(
            tail[0].opcode,
            Opcode::Combine {
                agg_parts: vec![AggPart::GroupKey, AggPart::Count, AggPart::Sum].into(),
                num_group_keys: 1,
                distinct: false,
                output_order: Vec::new().into(),
                project: Vec::new().into(),
            }
        );
        assert_eq!(
            tail[1].opcode,
            Opcode::Sort {
                keys: vec![(1, true)].into(),
                hidden: 0
            }
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_batch_compile_ce9ae325__v1_agg_without_group_by_emits_reduce() {
        // #452: a bare aggregate is a global `Reduce`, never a keyless
        // `GroupReduce` (which emits no row over zero surviving rows).
        let query = sql::parse("SELECT SUM(amount) FROM t").unwrap();
        let program = compile(&query).unwrap();
        let (body, ..) = program.split_finalize();
        assert!(body.iter().any(|op| matches!(op, Opcode::Reduce { .. })));
        assert!(!body
            .iter()
            .any(|op| matches!(op, Opcode::GroupReduce { .. })));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_batch_compile_ce9ae325__v2_group_by_without_agg_emits_group_reduce() {
        let query = sql::parse("SELECT region FROM t GROUP BY region").unwrap();
        let program = compile(&query).unwrap();
        let (body, ..) = program.split_finalize();
        assert!(body
            .iter()
            .any(|op| matches!(op, Opcode::GroupReduce { .. })));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_batch_compile_ce9ae325__v3_no_agg_no_group_by_omits_group_reduce() {
        let query = sql::parse("SELECT id FROM t").unwrap();
        let program = compile(&query).unwrap();
        let (body, ..) = program.split_finalize();
        assert!(!body
            .iter()
            .any(|op| matches!(op, Opcode::GroupReduce { .. })));
    }

    /// MC/DC vector (obligation `batch_1127`, `Combine`'s comment choice
    /// `group_by_present || has_agg`): leaf A true alone.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_batch_compile_3755607c__v1_group_by_without_agg_column_merges_partial_aggregates(
    ) {
        let query = sql::parse("SELECT region FROM t GROUP BY region").unwrap();
        let program = compile(&query).unwrap();
        let fin = program.instructions.last().unwrap();
        assert_eq!(fin.comment.as_deref(), Some("merge partial aggregates"));
    }

    /// MC/DC vector (obligation `batch_1127`): leaf B (`has_agg`) true alone.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_batch_compile_3755607c__v2_agg_column_without_group_by_merges_partial_aggregates(
    ) {
        let query = sql::parse("SELECT SUM(amount) FROM t").unwrap();
        let program = compile(&query).unwrap();
        let fin = program.instructions.last().unwrap();
        assert_eq!(fin.comment.as_deref(), Some("merge partial aggregates"));
    }

    /// MC/DC vector (obligation `batch_1127`): both leaves false --
    /// the plain concatenation comment.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_batch_compile_3755607c__v3_no_group_by_no_agg_column_concatenates_segments() {
        let query = sql::parse("SELECT id FROM t").unwrap();
        let program = compile(&query).unwrap();
        let fin = program.instructions.last().unwrap();
        assert_eq!(fin.comment.as_deref(), Some("concatenate segments"));
    }

    #[test]
    fn compile_join_splits_columns_by_table_and_builds_both_programs() {
        let query = sql::parse(
            "SELECT orders.id, regions.budget FROM orders JOIN regions ON orders.region_key = regions.rkey",
        )
        .unwrap();
        let plan = compile_join(&query, BuildSourceKind::InMemory).unwrap();
        assert_eq!(plan.left_columns, vec!["orders.id", "orders.region_key"]);
        assert_eq!(plan.right_columns, vec!["regions.budget", "regions.rkey"]);
        assert_eq!(plan.payload_dst, vec![2, 3]);
        assert!(matches!(
            plan.build.opcodes().last(),
            Some(Opcode::HashBuild { .. })
        ));
        assert!(matches!(
            plan.probe.opcodes().last(),
            Some(Opcode::HashProbe { .. })
        ));
        assert!(matches!(
            plan.body.opcodes().last(),
            Some(Opcode::Combine { .. })
        ));

        let bad = sql::parse("SELECT a.id FROM a JOIN b ON a.id = c.id").unwrap();
        assert_eq!(
            compile_join(&bad, BuildSourceKind::InMemory),
            Err(PlanError::UnknownColumn("c.id".into()))
        );
    }

    /// #771: `USING (k)` plans as `ON <from>.k = <joined>.k`, and `ON`
    /// plans the same with its operands in either order.
    #[test]
    fn compile_join_reads_using_and_either_operand_order_as_the_same_equality() {
        let plan = |sql: &str| compile_join(&sql::parse(sql).unwrap(), BuildSourceKind::InMemory);
        let on = plan(
            "SELECT orders.id, regions.budget FROM orders JOIN regions ON orders.k = regions.k",
        );
        assert!(on.is_ok(), "{on:?}");
        assert_eq!(
            plan("SELECT orders.id, regions.budget FROM orders JOIN regions USING (k)"),
            on
        );
        assert_eq!(
            plan(
                "SELECT orders.id, regions.budget FROM orders JOIN regions ON regions.k = orders.k"
            ),
            on
        );
        assert_eq!(
            plan("SELECT orders.id, regions.budget FROM orders LEFT JOIN regions USING (k)"),
            plan("SELECT orders.id, regions.budget FROM orders LEFT JOIN regions ON orders.k = regions.k")
        );
    }

    /// #771: `USING (k, l)` keys the hash on `k` and filters on `l`; an
    /// outer join cannot, and is refused.
    #[test]
    fn compile_join_filters_on_the_extra_using_columns_of_an_inner_join_only() {
        let plan = |sql: &str| compile_join(&sql::parse(sql).unwrap(), BuildSourceKind::InMemory);
        let using = plan("SELECT orders.id FROM orders JOIN regions USING (k, l)").unwrap();
        let single = plan("SELECT orders.id FROM orders JOIN regions USING (k)").unwrap();
        // The filter loads `l` from both tables on top of the key.
        assert!(using.left_columns.contains(&"orders.l".to_string()));
        assert!(using.right_columns.contains(&"regions.l".to_string()));
        assert!(!single.left_columns.contains(&"orders.l".to_string()));
        // The parser already refuses it; the planner refuses it too.
        let mut outer =
            sql::parse("SELECT orders.id FROM orders JOIN regions USING (k, l)").unwrap();
        outer.from.as_mut().unwrap().joins[0].op = JoinOp::Left;
        assert_eq!(
            compile_join(&outer, BuildSourceKind::InMemory),
            Err(PlanError::UnsupportedJoinKind(JoinOp::Left))
        );
    }

    /// #441: a join whose output feeds only a `GROUP BY`/aggregate fuses
    /// the probe and the reduce into one opcode instead of the ordinary
    /// `HashProbe` + separate `GroupReduce`.
    #[test]
    fn compile_join_fuses_probe_and_group_reduce_for_an_aggregate_only_body() {
        let query = sql::parse(
            "SELECT regions.tier, SUM(orders.amount) FROM orders \
             JOIN regions ON orders.region_key = regions.rkey \
             GROUP BY regions.tier",
        )
        .unwrap();
        let plan = compile_join(&query, BuildSourceKind::InMemory).unwrap();
        assert!(matches!(
            plan.probe.opcodes().last(),
            Some(Opcode::HashProbeGroupReduce { .. })
        ));
        assert!(!plan
            .probe
            .opcodes()
            .any(|op| matches!(op, Opcode::HashProbe { .. })));
        assert!(!plan
            .body
            .opcodes()
            .any(|op| matches!(op, Opcode::GroupReduce { .. })));
        let fused = plan.fused_group_by.expect("fusion should have engaged");
        assert_eq!(
            fused.len(),
            2,
            "one GROUP BY key register + one SUM register"
        );
        // The trimmed body must still end in the usual `Combine` (cross-
        // segment merge is completely unaffected by fusion).
        assert!(matches!(
            plan.body.opcodes().last(),
            Some(Opcode::Combine { .. })
        ));
    }

    /// #441: a plain (non-aggregate) join keeps the exact unfused shape --
    /// same `HashProbe`, same three-part `explain_opcodes` output as
    /// before this issue.
    #[test]
    fn compile_join_does_not_fuse_a_non_aggregate_body() {
        let query = sql::parse(
            "SELECT orders.id, regions.budget FROM orders \
             JOIN regions ON orders.region_key = regions.rkey",
        )
        .unwrap();
        let plan = compile_join(&query, BuildSourceKind::InMemory).unwrap();
        assert!(plan.fused_group_by.is_none());
        assert!(matches!(
            plan.probe.opcodes().last(),
            Some(Opcode::HashProbe { .. })
        ));
    }

    /// #441: a `WHERE` clause on the joined row (`Filter` between the
    /// probe's `LoadColumn`s and the `GroupReduce`) is exactly the
    /// "intervening row-shaping opcode" fusion declines to handle --
    /// falls back to the unfused path rather than fusing incorrectly.
    #[test]
    fn compile_join_does_not_fuse_when_a_filter_precedes_group_reduce() {
        let query = sql::parse(
            "SELECT regions.tier, SUM(orders.amount) FROM orders \
             JOIN regions ON orders.region_key = regions.rkey \
             WHERE orders.amount > 0 \
             GROUP BY regions.tier",
        )
        .unwrap();
        let plan = compile_join(&query, BuildSourceKind::InMemory).unwrap();
        assert!(plan.fused_group_by.is_none());
        assert!(matches!(
            plan.probe.opcodes().last(),
            Some(Opcode::HashProbe { .. })
        ));
    }

    #[test]
    fn compile_join_build_side_swaps_build_and_probe_when_from_table_must_build() {
        let query = sql::parse(
            "SELECT regions.budget, orders.id FROM regions \
             JOIN orders ON regions.rkey = orders.region_key",
        )
        .unwrap();
        let plan = compile_join_build_side(&query, "regions", BuildSourceKind::InMemory).unwrap();
        // `left_columns`/`right_columns` stay probe/build (not from/join):
        // `orders` (the JOIN target) now probes, `regions` (the FROM
        // table) now builds, even though it's written first (ADR-0021).
        assert_eq!(plan.left_columns, vec!["orders.id", "orders.region_key"]);
        assert_eq!(plan.right_columns, vec!["regions.budget", "regions.rkey"]);
        assert!(matches!(
            plan.build.opcodes().last(),
            Some(Opcode::HashBuild { .. })
        ));
        assert!(matches!(
            plan.probe.opcodes().last(),
            Some(Opcode::HashProbe { .. })
        ));
    }

    /// MC/DC vector (`compile_join_impl`'s `from_builds && join.op !=
    /// JoinOp::Inner` guard): both leaves true -- `LEFT JOIN` with the
    /// `build_table` named as the `FROM` table is rejected (would need
    /// `RIGHT JOIN` semantics, not implemented).
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_batch_compile_join_impl_ee5464fc__v1_from_builds_and_left_join_is_rejected() {
        let query = sql::parse(
            "SELECT regions.budget FROM regions \
             LEFT JOIN orders ON regions.rkey = orders.region_key",
        )
        .unwrap();
        assert_eq!(
            compile_join_build_side(&query, "regions", BuildSourceKind::InMemory),
            Err(PlanError::UnsupportedJoinKind(JoinOp::Left))
        );
    }

    /// MC/DC vector: leaf A (`from_builds`) true alone -- `INNER JOIN`
    /// with the `FROM` table forced to build is accepted (leaf B false).
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_batch_compile_join_impl_ee5464fc__v2_from_builds_and_inner_join_is_accepted() {
        let query = sql::parse(
            "SELECT regions.budget FROM regions \
             JOIN orders ON regions.rkey = orders.region_key",
        )
        .unwrap();
        assert!(compile_join_build_side(&query, "regions", BuildSourceKind::InMemory).is_ok());
    }

    /// MC/DC vector: leaf B (`join.op != Inner`) true alone -- a plain
    /// `LEFT JOIN` (`from_builds` false, the JOIN target still builds) is
    /// accepted, unaffected by the `build_table` override.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_batch_compile_join_impl_ee5464fc__v3_left_join_without_from_builds_is_accepted(
    ) {
        let query = sql::parse(
            "SELECT regions.budget FROM regions \
             LEFT JOIN orders ON regions.rkey = orders.region_key",
        )
        .unwrap();
        assert!(compile_join(&query, BuildSourceKind::InMemory).is_ok());
    }

    #[test]
    fn compile_semi_join_strips_the_in_clause_from_the_body() {
        let query =
            sql::parse("SELECT id FROM orders WHERE region_key IN (SELECT rkey FROM regions)")
                .unwrap();
        let plan = compile_semi_join(&query).unwrap();
        assert_eq!(plan.key_column, "region_key");
        assert_eq!(
            plan.subquery.from.as_ref().map(|f| table_name(f).unwrap()),
            Some("regions")
        );
        assert!(!plan
            .body
            .opcodes()
            .any(|op| matches!(op, Opcode::Filter { .. })));

        let plain = sql::parse("SELECT id FROM orders WHERE id > 1").unwrap();
        assert!(matches!(
            compile_semi_join(&plain),
            Err(PlanError::UnsupportedSemiJoin(_))
        ));
    }

    #[test]
    fn compile_window_emits_columns_and_window_registers_in_select_order() {
        let query = sql::parse(
            "SELECT id, ROW_NUMBER() OVER (PARTITION BY region_key ORDER BY id), region_key \
             FROM orders ORDER BY id",
        )
        .unwrap();
        let program = compile_window(&query).unwrap();
        assert_eq!(program.columns_to_load(), vec!["id", "region_key"]);
        let (body, combine, sort, limit) = program.split_finalize();
        assert!(matches!(combine, Some(Opcode::Combine { .. })));
        assert!(matches!(
            sort,
            Some(Opcode::Sort { keys, .. }) if keys[..] == [(0, false)]
        ));
        assert!(limit.is_none());
        assert!(body
            .iter()
            .any(|op| matches!(op, Opcode::Window { dst: 2, .. })));
        assert!(matches!(
            body.last(),
            Some(Opcode::Emit { registers }) if registers.as_ref() == [0, 2, 1]
        ));
    }

    fn details(nodes: &[PlanNode]) -> Vec<&str> {
        nodes.iter().map(|n| n.detail.as_str()).collect()
    }

    fn stats(_: &str) -> TableStats {
        TableStats {
            row_groups: 5,
            rows: 5000,
            source: None,
        }
    }

    #[test]
    fn explain_plain_filter_group_by_aggregate() {
        let query = sql::parse("SELECT region, SUM(amount), COUNT(*) FROM production WHERE id > 1000 GROUP BY region ORDER BY region").unwrap();
        let nodes = explain(&query, stats).unwrap();

        assert_eq!(nodes[0].detail, "QUERY PLAN");
        assert!(nodes[0].parent == nodes[0].id);
        assert!(details(&nodes).contains(&"SCAN production (5 row groups, ~5000 rows)"));
        assert!(details(&nodes).contains(&"LOAD COLUMNS: region, amount, id"));
        assert!(details(&nodes).contains(&"FILTER: id > 1000"));
        assert!(details(&nodes).contains(&"GROUP BY: region"));
        assert!(details(&nodes).contains(&"AGGREGATE: SUM(amount)"));
        assert!(details(&nodes).contains(&"AGGREGATE: COUNT(*)"));
        assert!(details(&nodes).contains(&"ORDER BY: region"));
        assert_eq!(
            nodes.last().unwrap().detail,
            "EMIT: region, SUM(amount), COUNT(*)"
        );

        let group_id = nodes
            .iter()
            .find(|n| n.detail == "GROUP BY: region")
            .unwrap()
            .id;
        let agg_parents: Vec<u32> = nodes
            .iter()
            .filter(|n| n.detail.starts_with("AGGREGATE"))
            .map(|n| n.parent)
            .collect();
        assert_eq!(agg_parents, vec![group_id, group_id]);
    }

    #[test]
    fn explain_join_and_semi_join_and_window() {
        let query = sql::parse("SELECT orders.id, regions.budget FROM orders JOIN regions ON orders.region_key = regions.rkey ORDER BY orders.id").unwrap();
        let nodes = explain(&query, stats).unwrap();
        assert!(details(&nodes).contains(&"LOAD COLUMNS: orders.id, orders.region_key"));
        assert!(details(&nodes).contains(&"LOAD COLUMNS: regions.budget, regions.rkey"));
        assert!(details(&nodes).contains(&"HASH JOIN: orders.region_key = regions.rkey"));

        let query = sql::parse(
            "SELECT id FROM orders WHERE region_key IN (SELECT rkey FROM regions) ORDER BY id",
        )
        .unwrap();
        let nodes = explain(&query, stats).unwrap();
        assert!(details(&nodes).contains(&"SEMI JOIN: region_key IN (SELECT rkey FROM regions)"));
        assert!(details(&nodes).contains(&"LOAD COLUMNS: id, region_key"));
        assert!(details(&nodes).contains(&"LOAD COLUMNS: rkey"));
        assert!(!details(&nodes).iter().any(|d| d.starts_with("FILTER")));

        let query = sql::parse(
            "SELECT id, region_key, ROW_NUMBER() OVER (PARTITION BY region_key ORDER BY id) \
             FROM orders ORDER BY id",
        )
        .unwrap();
        let nodes = explain(&query, stats).unwrap();
        assert!(details(&nodes)
            .contains(&"WINDOW: ROW_NUMBER() OVER (PARTITION BY region_key ORDER BY id)"));
        assert_eq!(
            nodes.last().unwrap().detail,
            "EMIT: id, region_key, ROW_NUMBER()"
        );
    }

    #[test]
    fn explain_shows_distinct_node_for_plain_select_distinct() {
        let query = sql::parse("SELECT DISTINCT region FROM production").unwrap();
        let nodes = explain(&query, stats).unwrap();
        assert!(details(&nodes).contains(&"DISTINCT"));
        assert_eq!(nodes.last().unwrap().detail, "EMIT: region");
    }

    #[test]
    fn explain_shows_distinct_after_group_by_and_aggregate() {
        let query =
            sql::parse("SELECT DISTINCT region, SUM(amount) FROM production GROUP BY region")
                .unwrap();
        let nodes = explain(&query, stats).unwrap();
        assert!(details(&nodes).contains(&"GROUP BY: region"));
        assert!(details(&nodes).contains(&"AGGREGATE: SUM(amount)"));
        assert!(details(&nodes).contains(&"DISTINCT"));

        // DISTINCT comes after GROUP BY/AGGREGATE, before EMIT.
        let group_pos = details(&nodes)
            .iter()
            .position(|d| *d == "GROUP BY: region")
            .unwrap();
        let distinct_pos = details(&nodes)
            .iter()
            .position(|d| *d == "DISTINCT")
            .unwrap();
        let emit_pos = details(&nodes).len() - 1;
        assert!(group_pos < distinct_pos);
        assert!(distinct_pos < emit_pos);
    }

    #[test]
    fn explain_omits_distinct_node_for_non_distinct_query() {
        let query = sql::parse("SELECT region FROM production").unwrap();
        let nodes = explain(&query, stats).unwrap();
        assert!(!details(&nodes).contains(&"DISTINCT"));
    }

    fn opcodes(rows: &[OpcodeRow]) -> Vec<&'static str> {
        rows.iter().map(|r| r.opcode).collect()
    }

    #[test]
    fn explain_opcodes_flat_query_matches_compiled_program() {
        let query =
            sql::parse("SELECT region, SUM(amount) FROM t WHERE amount > 10 GROUP BY region")
                .unwrap();
        let program = compile(&query).unwrap();
        let sections = explain_opcodes(&query).unwrap();

        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].label, "body");
        assert_eq!(
            opcodes(&sections[0].rows),
            program.opcodes().map(Opcode::name).collect::<Vec<_>>()
        );
        let finalize_row = sections[0].rows.iter().find(|r| r.is_finalize).unwrap();
        assert_eq!(finalize_row.opcode, "Combine");
        assert!(sections[0].rows.iter().filter(|r| r.is_finalize).count() == 1);
    }

    #[test]
    fn explain_opcodes_order_by_limit_matches_compiled_program() {
        let query = sql::parse("SELECT id FROM t ORDER BY id LIMIT 5").unwrap();
        let program = compile(&query).unwrap();
        let sections = explain_opcodes(&query).unwrap();

        assert_eq!(sections.len(), 1);
        assert_eq!(
            opcodes(&sections[0].rows),
            program.opcodes().map(Opcode::name).collect::<Vec<_>>()
        );
        let limit_row = sections[0]
            .rows
            .iter()
            .find(|r| r.opcode == "Limit")
            .unwrap();
        assert!(limit_row.operands.contains("n=5"));
    }

    #[test]
    fn explain_opcodes_join_lists_build_probe_and_body() {
        let query = sql::parse(
            "SELECT orders.id, regions.budget FROM orders JOIN regions ON orders.region_key = regions.rkey",
        )
        .unwrap();
        let join = compile_join(&query, BuildSourceKind::InMemory).unwrap();
        let sections = explain_opcodes(&query).unwrap();

        assert_eq!(sections.len(), 3);
        assert!(sections[0].label.starts_with("JOIN build"));
        assert_eq!(sections[1].label, "JOIN probe");
        assert_eq!(sections[2].label, "JOIN body");
        assert_eq!(
            opcodes(&sections[0].rows),
            join.build.opcodes().map(Opcode::name).collect::<Vec<_>>()
        );
        assert_eq!(
            opcodes(&sections[1].rows),
            join.probe.opcodes().map(Opcode::name).collect::<Vec<_>>()
        );
        assert_eq!(
            opcodes(&sections[2].rows),
            join.body.opcodes().map(Opcode::name).collect::<Vec<_>>()
        );
        assert!(sections[2].rows.iter().any(|r| r.is_finalize));
    }

    #[test]
    fn explain_opcodes_semi_join_lists_single_body_section() {
        let query =
            sql::parse("SELECT id FROM orders WHERE region_key IN (SELECT rkey FROM regions)")
                .unwrap();
        let semi = compile_semi_join(&query).unwrap();
        let sections = explain_opcodes(&query).unwrap();

        assert_eq!(sections.len(), 1);
        assert!(sections[0].label.contains("SEMI JOIN"));
        assert!(sections[0].label.contains("region_key"));
        assert!(sections[0].label.contains("regions"));
        assert_eq!(
            opcodes(&sections[0].rows),
            semi.body.opcodes().map(Opcode::name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn explain_opcodes_window_matches_compiled_program() {
        let query = sql::parse(
            "SELECT id, region_key, ROW_NUMBER() OVER (PARTITION BY region_key ORDER BY id) \
             FROM orders",
        )
        .unwrap();
        let program = compile_window(&query).unwrap();
        let sections = explain_opcodes(&query).unwrap();

        assert_eq!(sections.len(), 1);
        assert_eq!(
            opcodes(&sections[0].rows),
            program.opcodes().map(Opcode::name).collect::<Vec<_>>()
        );
        assert!(sections[0]
            .rows
            .iter()
            .any(|r| r.opcode == "Window" && r.operands.contains("func=RowNumber")));
    }

    #[test]
    fn expand_star_replaces_bare_star_with_schema_columns() {
        let query = sql::parse("SELECT * FROM t").unwrap();
        let schema = vec!["id".to_string(), "name".to_string(), "amount".to_string()];
        let expanded = expand_star(&query, &schema).unwrap();
        assert_eq!(
            output_column_names(&expanded),
            vec!["id".to_string(), "name".to_string(), "amount".to_string()]
        );
    }

    #[test]
    fn expand_star_keeps_mixed_columns_in_order() {
        let query = sql::parse("SELECT id, * FROM t").unwrap();
        let schema = vec!["id".to_string(), "name".to_string()];
        let expanded = expand_star(&query, &schema).unwrap();
        assert_eq!(
            output_column_names(&expanded),
            vec!["id".to_string(), "id".to_string(), "name".to_string()]
        );
    }

    #[test]
    fn expand_star_is_noop_without_star() {
        let query = sql::parse("SELECT id FROM t").unwrap();
        let expanded = expand_star(&query, &["id".to_string()]).unwrap();
        assert_eq!(expanded, query);
    }

    #[test]
    fn expand_star_rejects_group_by() {
        let query = sql::parse("SELECT * FROM t GROUP BY id").unwrap();
        assert_eq!(
            expand_star(&query, &["id".to_string()]),
            Err(PlanError::StarWithAggregation)
        );
    }

    #[test]
    fn expand_star_rejects_aggregate() {
        let query = sql::parse("SELECT *, SUM(amount) FROM t").unwrap();
        assert_eq!(
            expand_star(&query, &["id".to_string()]),
            Err(PlanError::StarWithAggregation)
        );
    }

    #[test]
    fn expand_star_rejects_window() {
        let query = sql::parse("SELECT *, ROW_NUMBER() OVER (ORDER BY id) FROM t").unwrap();
        assert_eq!(
            expand_star(&query, &["id".to_string()]),
            Err(PlanError::StarWithAggregation)
        );
    }

    #[test]
    fn expand_star_then_compile_projects_real_columns() {
        let query = sql::parse("SELECT * FROM t").unwrap();
        let schema = vec!["id".to_string(), "name".to_string()];
        let expanded = expand_star(&query, &schema).unwrap();
        assert_eq!(output_column_names(&expanded), vec!["id", "name"]);
        let program = compile(&expanded).unwrap();
        assert_eq!(program.columns_to_load(), vec!["id", "name"]);
    }

    // ---------------------------------------------------------------------
    // #67: FILTER (WHERE ...) execution semantics -- run the compiled
    // `Program` over real data via `vm::engine::run`, not just inspect
    // opcode shape, since the masking is only correct if `Reduce`/
    // `GroupReduce`/`Window`'s null-skipping actually excludes the right
    // rows.
    // ---------------------------------------------------------------------

    fn amount_batch() -> crate::vm::batch::Batch {
        crate::vm::batch::Batch::new(4)
            .with_column(
                "region",
                vec![
                    Value::Str("a".into()),
                    Value::Str("a".into()),
                    Value::Str("b".into()),
                    Value::Str("b".into()),
                ],
            )
            .with_column(
                "amount",
                vec![Value::Int(5), Value::Int(20), Value::Int(3), Value::Int(30)],
            )
    }

    fn run_program(program: &Program) -> Vec<Vec<Value>> {
        use crate::vm::engine::{run, InMemorySegment};
        run(&[InMemorySegment(amount_batch())], program)
            .unwrap()
            .into_rows()
    }

    // #585: grouped rows are merged keys-first; `Combine`'s `output_order`
    // puts them back in select-list order before DISTINCT/ORDER BY.

    fn run_grouped(sql_text: &str, segments: usize) -> Vec<Vec<Value>> {
        use crate::vm::engine::{run, InMemorySegment};
        let schema = ["region".to_string(), "amount".to_string()];
        let select = expand_star(&sql::parse(sql_text).unwrap(), &schema).unwrap();
        let program = compile(&select).unwrap();
        let segments: Vec<InMemorySegment> = (0..segments)
            .map(|_| InMemorySegment(amount_batch()))
            .collect();
        run(&segments, &program).unwrap().into_rows()
    }

    /// #644: `HAVING` filters the merged groups -- over keys, select-list
    /// aggregates, aggregates only it reads (alongside select-list ones,
    /// which must not be confused with them), aliases, `AVG` and
    /// `COUNT(DISTINCT)`; without `GROUP BY` over the one group, which it
    /// may filter out. Applied before `ORDER BY`/`LIMIT`. Over 1 segment and
    /// 256 the answers agree.
    #[test]
    fn having_filters_the_merged_groups() {
        let s = |v: &str| Value::Str(v.to_string().into());
        for segments in [1, 256] {
            let n = i64::try_from(segments).unwrap();
            let run = |q: &str| run_grouped(q, segments);
            // a: amounts 5, 20 (avg 12.5); b: 3, 30 (avg 16.5), per segment.
            assert_eq!(
                run("SELECT region, COUNT(*) FROM t GROUP BY region \
                     HAVING SUM(amount) / COUNT(*) > 13"),
                vec![vec![s("b"), Value::Int(2 * n)]],
            );
            assert_eq!(
                run("SELECT region, COUNT(*) FROM t GROUP BY region HAVING SUM(amount) > 30 * 256"),
                if segments == 256 {
                    vec![vec![s("b"), Value::Int(2 * n)]]
                } else {
                    vec![]
                },
            );
            assert_eq!(
                run("SELECT region, MAX(amount) AS m FROM t GROUP BY region HAVING m > 25"),
                vec![vec![s("b"), Value::Int(30)]],
            );
            assert_eq!(
                run("SELECT region FROM t GROUP BY region HAVING region = 'a'"),
                vec![vec![s("a")]],
            );
            assert_eq!(
                run("SELECT region FROM t GROUP BY region HAVING AVG(amount) < 15"),
                vec![vec![s("a")]],
            );
            assert_eq!(
                run(
                    "SELECT region, COUNT(DISTINCT amount) FROM t GROUP BY region \
                     HAVING COUNT(DISTINCT amount) = 2 AND MIN(amount) > 4"
                ),
                vec![vec![s("a"), Value::Int(2)]],
            );
            assert_eq!(
                run("SELECT COUNT(*) FROM t HAVING COUNT(*) > 100000"),
                Vec::<Vec<Value>>::new()
            );
            assert_eq!(
                run("SELECT SUM(amount) FROM t HAVING MIN(amount) = 3"),
                vec![vec![Value::Int(58 * n)]],
            );
            assert_eq!(
                run(
                    "SELECT region, SUM(amount) FROM t GROUP BY region HAVING COUNT(*) > 0 \
                     ORDER BY region DESC LIMIT 1"
                ),
                vec![vec![s("b"), Value::Int(33 * n)]],
            );
        }
    }

    /// #644: what `HAVING` refuses, in the select list's words.
    #[test]
    fn having_refusals() {
        let parse_err = |q: &str| sql::parse(q).unwrap_err().to_string();
        assert!(
            parse_err("SELECT region FROM t GROUP BY region HAVING amount > 1")
                .contains("column `amount` must be a GROUP BY key or inside an aggregate")
        );
        assert!(parse_err("SELECT region FROM t HAVING region = 'a'")
            .contains("HAVING requires GROUP BY or an aggregate"));
        assert!(
            parse_err("SELECT region FROM t GROUP BY region HAVING SUM(DISTINCT amount) > 1")
                .contains("only COUNT(DISTINCT x) is supported")
        );
        let schema = ["region".to_string(), "amount".to_string()];
        let select = expand_star(
            &sql::parse("SELECT region FROM t GROUP BY region HAVING COUNT(DISTINCT amount) > 1")
                .unwrap(),
            &schema,
        )
        .unwrap();
        assert!(matches!(
            compile(&select),
            Err(PlanError::UnsupportedSelectItem(m)) if m.contains("must also be in the SELECT list")
        ));
    }

    /// #679: an `IN (SELECT ...)` or `EXISTS` anywhere in WHERE, HAVING or
    /// the select list is refused by `compile` -- it used to compile to
    /// FALSE -- while a query without one plans as before.
    #[test]
    fn a_subquery_condition_anywhere_is_refused() {
        let schema = ["region".to_string(), "amount".to_string()];
        let plan = |q: &str| compile(&expand_star(&sql::parse(q).unwrap(), &schema).unwrap());
        for q in [
            "SELECT region FROM t WHERE amount > 0 AND amount IN (SELECT amount FROM t)",
            "SELECT region FROM t WHERE EXISTS (SELECT amount FROM t)",
            "SELECT region FROM t WHERE NOT (amount IN (SELECT amount FROM t))",
            "SELECT region FROM t WHERE amount IN (SELECT amount FROM t)",
        ] {
            let select = sql::parse(q).unwrap();
            assert!(has_subquery_condition(&select), "{q}");
            assert!(
                matches!(plan(q), Err(PlanError::UnsupportedSemiJoin(_))),
                "{q}"
            );
        }
        let plain =
            sql::parse("SELECT region FROM t WHERE amount > 0 OR region LIKE 'a%'").unwrap();
        assert!(!has_subquery_condition(&plain));
        assert!(plan("SELECT region FROM t WHERE amount > 0 OR region LIKE 'a%'").is_ok());
    }

    /// #679: `LIMIT n OFFSET m` is planned with its offset and skips `m`
    /// rows on every path -- the bounded scan (no ORDER BY), the top-N
    /// (one key), a multi-key sort and a grouped query -- over one segment
    /// and three.
    #[test]
    fn offset_is_planned_and_skips_rows_on_every_path() {
        let schema = ["a".to_string(), "b".to_string()];
        let select = expand_star(
            &sql::parse("SELECT a FROM t LIMIT 2 OFFSET 1").unwrap(),
            &schema,
        )
        .unwrap();
        let program = compile(&select).unwrap();
        let limit = program.instructions.last().unwrap();
        assert_eq!(limit.opcode, Opcode::Limit { n: 2, offset: 1 });
        assert_eq!(limit.comment.as_deref(), Some("LIMIT 2 OFFSET 1"));
        let i = |v: i64| Value::Int(v);
        for segments in [1, 3] {
            // Rows (a): 1, 1, NULL, 2, NULL -- in file order without ORDER BY.
            assert_eq!(
                run_ab("SELECT a FROM t LIMIT 2 OFFSET 1", segments),
                [[i(1)], [Value::Null]]
            );
            assert_eq!(
                run_ab("SELECT a FROM t ORDER BY a DESC LIMIT 2 OFFSET 1", segments),
                [[i(1)], [i(1)]]
            );
            assert_eq!(
                run_ab(
                    "SELECT a, b FROM t ORDER BY a, b LIMIT 1 OFFSET 1",
                    segments
                ),
                [[i(1), Value::Null]]
            );
            assert_eq!(
                run_ab(
                    "SELECT a, COUNT(*) FROM t GROUP BY a ORDER BY a LIMIT 5 OFFSET 2",
                    segments
                ),
                [[Value::Null, i(2)]]
            );
            assert!(run_ab("SELECT a FROM t LIMIT 2 OFFSET 9", segments).is_empty());
        }
    }

    /// #679: a window query's WHERE filters the rows before the windows
    /// see them -- it used to be dropped, numbering every row of the
    /// table. Expected rows from `duckdb` on the same five rows.
    #[test]
    fn window_functions_apply_where_first() {
        use crate::vm::engine::{run, InMemorySegment};
        let rows = [
            (Some(1), None),
            (Some(1), Some(2)),
            (None, Some(1)),
            (Some(2), None),
            (None, None),
        ];
        let cell = |v: Option<i64>| v.map_or(Value::Null, Value::Int);
        let batch = crate::vm::batch::Batch::new(rows.len())
            .with_column("a", rows.iter().map(|r| cell(r.0)).collect())
            .with_column("b", rows.iter().map(|r| cell(r.1)).collect());
        let select = sql::parse(
            "SELECT a, b, count(b) OVER (PARTITION BY a), row_number() OVER (PARTITION BY a ORDER BY b) \
             FROM t WHERE b IS NOT NULL OR a = 2",
        )
        .unwrap();
        let program = compile_window(&select).unwrap();
        assert!(program
            .instructions
            .iter()
            .any(|i| matches!(i.opcode, Opcode::Filter { .. })));
        let mut got = run(&[InMemorySegment(batch)], &program)
            .unwrap()
            .into_rows();
        got.sort_by_key(|r| format!("{r:?}"));
        let i = |v: i64| Value::Int(v);
        let mut want = vec![
            vec![i(1), i(2), i(1), i(1)],
            vec![i(2), Value::Null, i(0), i(1)],
            vec![Value::Null, i(1), i(1), i(1)],
        ];
        want.sort_by_key(|r| format!("{r:?}"));
        assert_eq!(got, want);
    }

    /// `(a, b)` rows with NULLs in both, split over `segments` segments.
    fn run_ab(sql_text: &str, segments: usize) -> Vec<Vec<Value>> {
        use crate::vm::engine::{run, InMemorySegment};
        let rows = [
            (Some(1), None),
            (Some(1), Some(2)),
            (None, Some(1)),
            (Some(2), None),
            (None, None),
        ];
        let schema = ["a".to_string(), "b".to_string()];
        let select = expand_star(&sql::parse(sql_text).unwrap(), &schema).unwrap();
        let program = compile(&select).unwrap();
        let cell = |v: Option<i64>| v.map_or(Value::Null, Value::Int);
        let segs: Vec<InMemorySegment> = rows
            .chunks(rows.len().div_ceil(segments))
            .map(|part| {
                InMemorySegment(
                    crate::vm::batch::Batch::new(part.len())
                        .with_column("a", part.iter().map(|r| cell(r.0)).collect())
                        .with_column("b", part.iter().map(|r| cell(r.1)).collect()),
                )
            })
            .collect();
        run(&segs, &program).unwrap().into_rows()
    }

    /// #645: `ORDER BY` with several terms sorts by each in turn, NULLs
    /// last for every term in either direction (DuckDB's default, checked
    /// against `duckdb` with these rows), over one segment and three.
    #[test]
    fn order_by_several_terms_sorts_by_each_in_turn() {
        let i = |v: i64| Value::Int(v);
        let n = Value::Null;
        for segments in [1, 3] {
            assert_eq!(
                run_ab("SELECT a, b FROM t ORDER BY a DESC, b", segments),
                vec![
                    vec![i(2), n.clone()],
                    vec![i(1), i(2)],
                    vec![i(1), n.clone()],
                    vec![n.clone(), i(1)],
                    vec![n.clone(), n.clone()],
                ],
            );
            assert_eq!(
                run_ab("SELECT a, b FROM t ORDER BY a, b DESC", segments),
                vec![
                    vec![i(1), i(2)],
                    vec![i(1), n.clone()],
                    vec![i(2), n.clone()],
                    vec![n.clone(), i(1)],
                    vec![n.clone(), n.clone()],
                ],
            );
            // A key the select list does not show sorts, then is dropped.
            assert_eq!(
                run_ab("SELECT b FROM t ORDER BY a DESC, b", segments),
                vec![
                    vec![n.clone()],
                    vec![i(2)],
                    vec![n.clone()],
                    vec![i(1)],
                    vec![n.clone()]
                ],
            );
            assert_eq!(
                run_ab("SELECT b FROM t ORDER BY a, b DESC LIMIT 2", segments),
                vec![vec![i(2)], vec![n.clone()]],
            );
            assert_eq!(
                run_ab("SELECT a FROM t ORDER BY b LIMIT 1", segments),
                vec![vec![n.clone()]],
            );
        }
        for segments in [1, 256] {
            let k = i64::try_from(segments).unwrap();
            let s = |v: &str| Value::Str(v.to_string().into());
            // Grouped: an aggregate term, then a key.
            assert_eq!(
                run_grouped(
                    "SELECT region, COUNT(*), MIN(amount) FROM t GROUP BY region \
                     ORDER BY COUNT(*) DESC, MIN(amount)",
                    segments
                ),
                vec![
                    vec![s("b"), Value::Int(2 * k), Value::Int(3)],
                    vec![s("a"), Value::Int(2 * k), Value::Int(5)],
                ],
            );
            // `COUNT(DISTINCT x)` resolves to its select item.
            assert_eq!(
                run_grouped(
                    "SELECT region, COUNT(DISTINCT amount) FROM t GROUP BY region \
                     ORDER BY COUNT(DISTINCT amount), region DESC",
                    segments
                ),
                vec![vec![s("b"), Value::Int(2)], vec![s("a"), Value::Int(2)]],
            );
        }
    }

    /// #645: `ORDER BY` refusals, which used to drop the sort silently,
    /// and the plan text.
    #[test]
    fn order_by_refusals_and_plan_text() {
        let schema = ["region".to_string(), "amount".to_string()];
        let plan = |q: &str| compile(&expand_star(&sql::parse(q).unwrap(), &schema).unwrap());
        assert!(matches!(
            plan("SELECT region, COUNT(*) FROM t GROUP BY region ORDER BY amount"),
            Err(PlanError::UnsupportedSelectItem(m)) if m.contains("grouped or aggregate")
        ));
        assert!(matches!(
            plan("SELECT region, COUNT(*) FROM t GROUP BY region ORDER BY SUM(amount)"),
            Err(PlanError::UnsupportedSelectItem(_))
        ));
        assert!(matches!(
            plan("SELECT DISTINCT region FROM t ORDER BY amount"),
            Err(PlanError::UnsupportedSelectItem(m)) if m.contains("SELECT DISTINCT")
        ));
        let program = plan("SELECT region FROM t ORDER BY region DESC, amount").unwrap();
        let sort = program
            .instructions
            .iter()
            .find(|i| matches!(i.opcode, Opcode::Sort { .. }))
            .unwrap();
        assert_eq!(
            sort.comment.as_deref(),
            Some("ORDER BY region DESC, amount")
        );
        assert!(
            matches!(&sort.opcode, Opcode::Sort { keys, hidden: 1 } if keys[..] == [(0, true), (1, false)])
        );
    }

    fn sorted(mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
        rows.sort_by_key(|r| format!("{r:?}"));
        rows
    }

    /// #623: grouped select items are functions of their group. `GROUP
    /// BY amount` groups by the real column even though a select item is
    /// aliased `amount`; expressions over keys and aggregates are computed
    /// after the merge, with integer literals keeping integer results.
    /// Over 1 segment (merged directly) and 256 (pre-aggregated per
    /// worker) the answers agree.
    #[test]
    fn grouped_select_items_are_functions_of_the_group() {
        for segments in [1, 256] {
            let n = i64::try_from(segments).unwrap();
            let s = |v: &str| Value::Str(v.to_string().into());
            // A real column wins over a same-named alias (sqlite, DuckDB).
            assert_eq!(
                sorted(run_grouped(
                    "SELECT amount - amount AS amount, COUNT(*) FROM t GROUP BY amount",
                    segments
                )),
                vec![vec![Value::Int(0), Value::Int(n)]; 4],
            );
            // A name that is only an alias still groups by its expression (#307).
            assert_eq!(
                run_grouped(
                    "SELECT amount * 10 AS k, COUNT(*) FROM t GROUP BY k ORDER BY k",
                    segments
                ),
                vec![
                    vec![Value::Int(30), Value::Int(n)],
                    vec![Value::Int(50), Value::Int(n)],
                    vec![Value::Int(200), Value::Int(n)],
                    vec![Value::Int(300), Value::Int(n)],
                ],
            );
            // An expression over a key; the key itself left out.
            assert_eq!(
                run_grouped(
                    "SELECT region || '!' AS r, COUNT(*) FROM t GROUP BY region ORDER BY r",
                    segments
                ),
                vec![
                    vec![s("a!"), Value::Int(2 * n)],
                    vec![s("b!"), Value::Int(2 * n)],
                ],
            );
            assert_eq!(
                run_grouped("SELECT COUNT(*) FROM t GROUP BY region", segments),
                vec![vec![Value::Int(2 * n)]; 2],
            );
            // Expressions over aggregates and keys stay integers.
            assert_eq!(
                run_grouped(
                    "SELECT region, SUM(amount) * 2, SUM(amount) + 1, \
                     MAX(amount) - MIN(amount) + COUNT(*) FROM t \
                     GROUP BY region ORDER BY region",
                    segments
                ),
                vec![
                    vec![
                        s("a"),
                        Value::Int(50 * n),
                        Value::Int(25 * n + 1),
                        Value::Int(15 + 2 * n)
                    ],
                    vec![
                        s("b"),
                        Value::Int(66 * n),
                        Value::Int(33 * n + 1),
                        Value::Int(27 + 2 * n)
                    ],
                ],
            );
            assert_eq!(
                run_grouped("SELECT SUM(amount) + SUM(amount) * 2 FROM t", segments),
                vec![vec![Value::Int(174 * n)]],
            );
        }
    }

    /// #623: a column read outside an aggregate must be a `GROUP BY` key.
    #[test]
    fn an_ungrouped_column_is_refused() {
        let err = sql::parse("SELECT amount, COUNT(*) FROM t GROUP BY region").unwrap_err();
        assert!(
            err.to_string()
                .contains("column `amount` must be a GROUP BY key or inside an aggregate"),
            "{err}"
        );
        let err = sql::parse("SELECT SUM(amount) + amount FROM t GROUP BY region").unwrap_err();
        assert!(err.to_string().contains("column `amount`"), "{err}");
    }

    /// #623 review: a `GROUP BY` name that resolves to an aggregate's
    /// alias is refused (DuckDB: "GROUP BY clause cannot contain
    /// aggregates"); a real column of that name still wins.
    #[test]
    fn group_by_an_aggregate_alias_is_refused() {
        let schema = ["region".to_string(), "amount".to_string()];
        let query = sql::parse("SELECT SUM(amount) AS s, COUNT(*) FROM t GROUP BY s").unwrap();
        let err = expand_star(&query, &schema).unwrap_err();
        assert!(
            err.to_string()
                .contains("GROUP BY cannot contain aggregates"),
            "{err}"
        );
        assert_eq!(
            sorted(run_grouped(
                "SELECT SUM(amount) AS amount, COUNT(*) FROM t GROUP BY amount",
                1
            )),
            vec![
                vec![Value::Int(20), Value::Int(1)],
                vec![Value::Int(3), Value::Int(1)],
                vec![Value::Int(30), Value::Int(1)],
                vec![Value::Int(5), Value::Int(1)],
            ],
        );
    }

    /// #623 review: `resolve_group_by` swaps in an alias's expression only
    /// for a name that is not a column, and leaves qualified names and
    /// expressions alone.
    #[test]
    fn resolve_group_by_prefers_a_column_over_an_alias() {
        let query = sql::parse(
            "SELECT region || '!' AS region, amount * 2 AS k, COUNT(*) FROM t \
             GROUP BY region, k",
        )
        .unwrap();
        let resolved = resolve_group_by(&query, &["region".to_string()]).unwrap();
        assert_eq!(
            expr_column_name(&resolved.group_by[0]).as_deref(),
            Some("region")
        );
        assert_eq!(expr_to_string(&resolved.group_by[1]), "amount * 2");
        // With no schema the same name is the alias.
        let resolved = resolve_group_by(&query, &[]).unwrap();
        assert_eq!(expr_to_string(&resolved.group_by[0]), "region || '!'");
    }

    /// #623 review: each projection shape over 1 and 256 segments: `LIKE`
    /// over an aggregate (was a planner invariant error), `COUNT(DISTINCT)`
    /// with its key computed on or left out (was refused), `AVG`'s two
    /// partials taking one merged column, and a column grouped by its own
    /// alias.
    #[test]
    fn projection_covers_like_count_distinct_avg_and_aliases() {
        for segments in [1, 256] {
            let n = i64::try_from(segments).unwrap();
            let s = |v: &str| Value::Str(v.to_string().into());
            assert_eq!(
                run_grouped(
                    "SELECT region, MIN(region) LIKE 'a', COUNT(*) FROM t \
                     GROUP BY region ORDER BY region",
                    segments
                ),
                vec![
                    vec![s("a"), Value::Bool(true), Value::Int(2 * n)],
                    vec![s("b"), Value::Bool(false), Value::Int(2 * n)],
                ],
            );
            assert_eq!(
                run_grouped(
                    "SELECT COUNT(DISTINCT amount) FROM t GROUP BY region",
                    segments
                ),
                vec![vec![Value::Int(2)]; 2],
            );
            assert_eq!(
                run_grouped(
                    "SELECT region || '!' AS r, COUNT(DISTINCT amount), SUM(amount) * 2 \
                     FROM t GROUP BY region ORDER BY r",
                    segments
                ),
                vec![
                    vec![s("a!"), Value::Int(2), Value::Int(50 * n)],
                    vec![s("b!"), Value::Int(2), Value::Int(66 * n)],
                ],
            );
            assert_eq!(
                run_grouped(
                    "SELECT AVG(amount) + 0, MAX(amount) - MIN(amount), region \
                     FROM t GROUP BY region ORDER BY region",
                    segments
                ),
                vec![
                    vec![Value::Float(12.5), Value::Int(15), s("a")],
                    vec![Value::Float(16.5), Value::Int(27), s("b")],
                ],
            );
            assert_eq!(
                run_grouped(
                    "SELECT region AS r, COUNT(*) FROM t GROUP BY r ORDER BY r",
                    segments
                ),
                vec![
                    vec![s("a"), Value::Int(2 * n)],
                    vec![s("b"), Value::Int(2 * n)]
                ],
            );
            // DISTINCT applies to the projected rows.
            assert_eq!(
                run_grouped(
                    "SELECT DISTINCT COUNT(*) + 0 FROM t GROUP BY region",
                    segments
                ),
                vec![vec![Value::Int(2 * n)]],
            );
        }
    }

    /// #623 review: NULL keys group together and project as NULL; over no
    /// rows a global aggregate expression is one row and a grouped one is
    /// none.
    #[test]
    fn projection_over_null_keys_and_empty_input() {
        use crate::vm::engine::{run, InMemorySegment};
        let schema = ["region".to_string(), "amount".to_string()];
        let program =
            |q: &str| compile(&expand_star(&sql::parse(q).unwrap(), &schema).unwrap()).unwrap();
        let batch = crate::vm::batch::Batch::new(3)
            .with_column(
                "region",
                vec![Value::Str("a".into()), Value::Null, Value::Null],
            )
            .with_column("amount", vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        let rows = run(
            &[InMemorySegment(batch)],
            &program("SELECT region || '!' AS r, SUM(amount) FROM t GROUP BY region"),
        )
        .unwrap()
        .into_rows();
        assert_eq!(
            sorted(rows),
            vec![
                vec![Value::Null, Value::Int(5)],
                vec![Value::Str("a!".into()), Value::Int(1)],
            ],
        );
        let none: [InMemorySegment; 0] = [];
        assert_eq!(
            run(
                &none,
                &program("SELECT SUM(amount) * 2, COUNT(*) + 1 FROM t")
            )
            .unwrap()
            .into_rows(),
            vec![vec![Value::Null, Value::Int(1)]],
        );
        assert!(run(
            &none,
            &program("SELECT COUNT(*) + 1 FROM t GROUP BY region")
        )
        .unwrap()
        .into_rows()
        .is_empty());
    }

    /// #623 review: a projection refuses a leftover column even when it is
    /// named like a merged column, and refuses a subquery.
    #[test]
    fn projection_refuses_a_leftover_column_named_like_a_merged_one() {
        // Unvalidated: the column parser would refuse `#0` itself.
        let crate::parser::row::ParseOutcome::Accepted(select) =
            crate::parser::row::parse_select_with_subscripts(
                "SELECT \"#0\", COUNT(*) FROM t GROUP BY region",
            )
        else {
            panic!("expected the row parser to accept the query");
        };
        let err = compile(&select).unwrap_err();
        assert!(
            err.to_string()
                .contains("column `#0` must be a GROUP BY key or inside an aggregate"),
            "{err}"
        );
    }

    /// #623 review: the projection shows in EXPLAIN, keeps the program off
    /// the streaming and bounded-scan paths, and is absent when the select
    /// list is exactly the keys then plain aggregates.
    #[test]
    fn projection_shows_in_explain_and_disables_streaming() {
        let schema = ["region".to_string(), "amount".to_string()];
        let program =
            |q: &str| compile(&expand_star(&sql::parse(q).unwrap(), &schema).unwrap()).unwrap();
        let projected = program("SELECT SUM(amount) * 2 FROM t GROUP BY region");
        let rendered: Vec<String> = render_program(&projected)
            .into_iter()
            .map(|row| format!("{row:?}"))
            .collect();
        assert!(
            rendered.iter().any(|r| r.contains("project=[")),
            "{rendered:?}"
        );
        assert!(!crate::vm::engine::is_streamable(&projected));
        let plain = program("SELECT region, SUM(amount) FROM t GROUP BY region");
        assert!(plain
            .opcodes()
            .any(|op| matches!(op, Opcode::Combine { project, .. } if project.is_empty())));
        // A hand-built LIMIT scan whose Combine projects is no prefix scan.
        let mut scan = program("SELECT amount FROM t LIMIT 10");
        assert_eq!(bounded_scan_limit_of(&scan), Some(10));
        for instruction in &mut scan.instructions {
            if let Opcode::Combine { project, .. } = &mut instruction.opcode {
                *project = vec![Opcode::Emit {
                    registers: vec![0].into(),
                }]
                .into();
            }
        }
        assert_eq!(bounded_scan_limit_of(&scan), None);
    }

    /// A constant loaded after a `Map` already applied `WHERE`'s selection
    /// (`amount * 3 - 1`: the `- 1` loads after `* 3` compacted the rows)
    /// is as long as the filtered rows, not the batch (was
    /// `RegisterLengthMismatch`).
    #[test]
    fn a_constant_after_the_filter_is_applied_has_the_filtered_length() {
        assert_eq!(
            run_grouped("SELECT amount * 3 - 1 FROM t WHERE amount > 4", 1),
            vec![
                vec![Value::Int(14)],
                vec![Value::Int(59)],
                vec![Value::Int(89)],
            ],
        );
    }

    /// Runs `sql_text` over one segment per batch, each with `region` and
    /// `amount` columns.
    fn run_batches(
        sql_text: &str,
        batches: Vec<crate::vm::batch::Batch>,
    ) -> std::result::Result<Vec<Vec<Value>>, crate::vm::batch::VmError> {
        use crate::vm::engine::{run, InMemorySegment};
        let schema = ["region".to_string(), "amount".to_string()];
        let select = expand_star(&sql::parse(sql_text).unwrap(), &schema).unwrap();
        let program = compile(&select).unwrap();
        let segments: Vec<InMemorySegment> = batches.into_iter().map(InMemorySegment).collect();
        run(&segments, &program).map(|o| o.into_rows())
    }

    fn amounts(values: Vec<Value>) -> crate::vm::batch::Batch {
        crate::vm::batch::Batch::new(values.len())
            .with_column("region", vec![Value::Str("a".into()); values.len()])
            .with_column("amount", values)
    }

    /// #643: a segment longer than `SLICE_ROWS` runs in slices; the answer
    /// (group order without ORDER BY included) is the same as over the
    /// same rows split into small segments, which never slice. Typed
    /// integer, dictionary and string columns all go through the slicing.
    #[test]
    fn a_long_segment_runs_in_slices_with_the_same_answer() {
        use crate::vm::batch::SLICE_ROWS;
        let n = SLICE_ROWS * 2 + 7_000;
        let regions = ["a", "b", "c"];
        let batch = |range: std::ops::Range<usize>| {
            let rows = range.len();
            crate::vm::batch::Batch::new(rows)
                .with_typed_column(
                    "region",
                    crate::vm::column::Column::Dict {
                        dict: regions.iter().map(|r| std::sync::Arc::from(*r)).collect(),
                        indices: range
                            .clone()
                            .map(|i| u32::try_from((i / 100) % 3).unwrap())
                            .collect(),
                        valid: crate::vm::column::Bitmap::new(rows, true),
                    },
                )
                .with_typed_column(
                    "amount",
                    crate::vm::column::Column::from(
                        range
                            .map(|i| {
                                if i % 97 == 0 {
                                    Value::Null
                                } else {
                                    Value::Int(i64::try_from(i % 1000).unwrap())
                                }
                            })
                            .collect::<Vec<_>>(),
                    ),
                )
        };
        let whole = || vec![batch(0..n)];
        let pieces = || {
            (0..n)
                .step_by(1000)
                .map(|s| batch(s..(s + 1000).min(n)))
                .collect()
        };
        for sql in [
            "SELECT region, COUNT(*), SUM(amount), MIN(amount), MAX(amount), AVG(amount) \
             FROM t WHERE amount > 100 GROUP BY region",
            "SELECT amount, COUNT(*) FROM t GROUP BY amount",
            "SELECT region, COUNT(DISTINCT amount) FROM t GROUP BY region",
            "SELECT COUNT(*), SUM(amount) FROM t WHERE region = 'b'",
            "SELECT region, amount FROM t WHERE amount = 999",
        ] {
            assert_eq!(
                run_batches(sql, whole()).unwrap(),
                run_batches(sql, pieces()).unwrap(),
                "{sql}"
            );
        }
        assert_eq!(
            run_batches("SELECT COUNT(*) FROM t", whole()).unwrap(),
            vec![vec![Value::Int(i64::try_from(n).unwrap())]]
        );
    }

    /// #655: the column engine runs an expression at the height limit
    /// (1000) -- validation, planning and the VM all recurse over it. Its
    /// own thread: debug frames are about ten times release ones (see
    /// the row engine's twin in `sqlite_parity_test`).
    #[test]
    fn an_expression_at_the_height_limit_runs_in_the_column_engine() {
        std::thread::Builder::new()
            .stack_size(256 << 20)
            .spawn(|| {
                let sum = vec!["amount"; 1000].join(" + ");
                let filter = vec!["amount > 0"; 999].join(" AND ");
                let rows = run_batches(
                    &format!("SELECT {sum} FROM t WHERE {filter}"),
                    vec![amounts(vec![Value::Int(2)])],
                )
                .unwrap();
                assert_eq!(rows, vec![vec![Value::Int(2000)]]);
                let over = format!("SELECT {} FROM t", vec!["amount"; 1001].join(" + "));
                let err = sql::parse(&over).unwrap_err();
                assert!(
                    err.to_string().contains("Expression tree is too large"),
                    "{err}"
                );
            })
            .unwrap()
            .join()
            .unwrap();
    }

    /// #635: an integer overflow is a query error -- in a row expression
    /// (also when only a later segment overflows), and in an expression
    /// over an aggregate -- never a wrapped or saturated value. A row the
    /// filter drops never overflows.
    #[test]
    fn integer_overflow_is_a_query_error() {
        let max = || amounts(vec![Value::Int(i64::MAX)]);
        for sql in ["SELECT amount + 1 FROM t", "SELECT amount * 2 FROM t"] {
            let err = run_batches(sql, vec![amounts(vec![Value::Int(1)]), max()]).unwrap_err();
            assert!(err.to_string().contains("integer overflow"), "{sql}: {err}");
        }
        let err = run_batches("SELECT MAX(amount) + 1 FROM t", vec![max(), max()]).unwrap_err();
        assert!(err.to_string().contains("integer overflow"), "{err}");
        assert!(
            run_batches("SELECT amount + 1 FROM t WHERE amount < 0", vec![max()])
                .unwrap()
                .is_empty()
        );
    }

    /// #635: arithmetic over a decimal `SUM` stays a decimal at DuckDB's
    /// scale; `/` is a float.
    #[test]
    fn decimal_sum_projection_keeps_its_scale() {
        let d = |unscaled| Value::Decimal { unscaled, scale: 2 };
        let rows = run_batches(
            "SELECT SUM(amount) + 1, SUM(amount) * 2, -SUM(amount), SUM(amount) / 2 FROM t",
            vec![amounts(vec![d(110)]), amounts(vec![d(220)])],
        )
        .unwrap();
        assert_eq!(
            rows,
            vec![vec![d(430), d(660), d(-330), Value::Float(1.65)]]
        );
        assert_eq!(rows[0][1].to_string(), "6.60");
    }

    /// #635: a constant loaded after the selection resolved is sized per
    /// segment -- segments keep different numbers of rows, one keeps none.
    #[test]
    fn a_constant_after_the_filter_is_sized_per_segment() {
        let ints = |v: &[i64]| amounts(v.iter().map(|&x| Value::Int(x)).collect());
        let rows = run_batches(
            "SELECT amount * 3 - 1 FROM t WHERE amount > 10",
            vec![ints(&[11, 2, 12, 13]), ints(&[1, 2]), ints(&[20])],
        )
        .unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(32)],
                vec![Value::Int(35)],
                vec![Value::Int(38)],
                vec![Value::Int(59)],
            ]
        );
    }

    /// An aggregate over the `GROUP BY` key itself: `GroupReduce` wrote
    /// the per-group keys back into the key register before reading the
    /// aggregates' sources, so `MIN(region)` saw 2 keys against 4 rows
    /// (`RegisterLengthMismatch` on untyped input).
    #[test]
    fn an_aggregate_over_the_group_key_reads_the_rows() {
        let s = |v: &str| Value::Str(v.to_string().into());
        for segments in [1, 256] {
            let n = i64::try_from(segments).unwrap();
            assert_eq!(
                run_grouped(
                    "SELECT region, MIN(region), COUNT(*) FROM t GROUP BY region ORDER BY region",
                    segments
                ),
                vec![
                    vec![s("a"), s("a"), Value::Int(2 * n)],
                    vec![s("b"), s("b"), Value::Int(2 * n)],
                ],
            );
        }
    }

    fn bounded_scan_limit_of(program: &Program) -> Option<usize> {
        crate::vm::engine::bounded_scan_limit(program)
    }

    /// #633: every segment holds the same rows, so a distinct count is
    /// the same over 1 segment (merged directly) as over 256 (pre-
    /// aggregated per worker), while `COUNT(*)` grows with them.
    #[test]
    fn count_distinct_dedups_across_segments_and_workers() {
        for segments in [1, 256] {
            let n = i64::try_from(segments).unwrap();
            let s = |v: &str| Value::Str(v.to_string().into());
            assert_eq!(
                run_grouped(
                    "SELECT region, COUNT(DISTINCT amount), COUNT(*) FROM t \
                     GROUP BY region ORDER BY region",
                    segments
                ),
                vec![
                    vec![s("a"), Value::Int(2), Value::Int(2 * n)],
                    vec![s("b"), Value::Int(2), Value::Int(2 * n)],
                ],
            );
            // An expression argument, the key column itself, a FILTER
            // (masked rows become NULL, which is not counted).
            assert_eq!(
                run_grouped(
                    "SELECT COUNT(DISTINCT amount * 0), SUM(amount) FROM t",
                    segments
                ),
                vec![vec![Value::Int(1), Value::Int(58 * n)]],
            );
            assert_eq!(
                run_grouped("SELECT COUNT(DISTINCT region) FROM t", segments),
                vec![vec![Value::Int(2)]],
            );
            assert_eq!(
                run_grouped(
                    "SELECT region, COUNT(DISTINCT region) FROM t GROUP BY region ORDER BY region",
                    segments
                ),
                vec![vec![s("a"), Value::Int(1)], vec![s("b"), Value::Int(1)]],
            );
            assert_eq!(
                run_grouped(
                    "SELECT COUNT(DISTINCT amount) FILTER (WHERE amount > 4) FROM t",
                    segments
                ),
                vec![vec![Value::Int(3)]],
            );
            assert_eq!(
                run_grouped(
                    "SELECT region, COUNT(DISTINCT amount) AS n FROM t GROUP BY region \
                     ORDER BY COUNT(DISTINCT amount) DESC",
                    segments
                )
                .len(),
                2,
            );
        }
    }

    fn first_item(sql_text: &str, distinct: bool) -> Result<Item> {
        let mut select = sql::parse(sql_text).unwrap();
        if let ResultColumn::Expr { expr, .. } = &mut select.columns[0] {
            if let ExprKind::FunctionCall { distinct: d, .. } = &mut expr.kind {
                *d = distinct;
            }
        }
        classify_item(&select.columns[0])
    }

    // codegen_batch_classify_item_96783126 (`classify_item`):
    // `*distinct && (agg != AggFunc::Count || arg.is_none())`
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_batch_classify_item_96783126__v1_sum_distinct_is_refused() {
        assert!(first_item("SELECT SUM(amount) FROM t", true).is_err());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_batch_classify_item_96783126__v2_a_plain_aggregate_is_kept() {
        assert!(matches!(
            first_item("SELECT SUM(amount) FROM t", false),
            Ok(Item::Agg(AggFunc::Sum, _, _, false))
        ));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_batch_classify_item_96783126__v3_count_distinct_of_a_column_is_kept() {
        assert!(matches!(
            first_item("SELECT COUNT(amount) FROM t", true),
            Ok(Item::Agg(AggFunc::Count, Some(_), _, true))
        ));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_batch_classify_item_96783126__v4_count_distinct_star_is_refused() {
        assert!(first_item("SELECT COUNT(*) FROM t", true).is_err());
    }

    fn having_reuse_of(sql_text: &str) -> Result<Vec<Option<(usize, usize)>>> {
        let select = sql::parse(sql_text).unwrap();
        let items: Vec<Item> = select
            .columns
            .iter()
            .map(|c| classify_item(c).unwrap())
            .collect();
        let having = select.having.as_ref().unwrap();
        let calls = agg_expr_calls(having);
        having_reuse(&calls, &select, &items)
    }

    // codegen_batch_having_reuse_14d24c6f (`having_reuse`):
    // `distinct && found.is_none()`
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_batch_having_reuse_14d24c6f__v1_distinct_not_in_select_is_refused() {
        assert!(having_reuse_of(
            "SELECT region, SUM(amount) FROM t GROUP BY region HAVING COUNT(DISTINCT amount) > 1"
        )
        .is_err());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_batch_having_reuse_14d24c6f__v2_plain_call_not_in_select_gets_own_column() {
        let reuse = having_reuse_of(
            "SELECT region, SUM(amount) FROM t GROUP BY region HAVING MIN(amount) > 1",
        )
        .unwrap();
        assert_eq!(reuse, vec![None]);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_batch_having_reuse_14d24c6f__v3_distinct_in_select_is_reused() {
        let reuse = having_reuse_of(
            "SELECT region, COUNT(DISTINCT amount) FROM t GROUP BY region \
             HAVING COUNT(DISTINCT amount) > 1",
        )
        .unwrap();
        assert_eq!(reuse, vec![Some((1, 0))]);
    }

    /// Compiles `name(<arity args>)`, built by renaming/resizing a parsed
    /// two-argument call (the row parser itself rejects `ilike(...)`).
    fn compile_call(name: &str, arity: usize) -> Vec<Instruction> {
        let mut select = sql::parse("SELECT json_extract(a, 'x') FROM t").unwrap();
        let ResultColumn::Expr { expr, .. } = &mut select.columns[0] else {
            panic!("expected an expression column");
        };
        let ExprKind::FunctionCall {
            name: n,
            args: FunctionArgs::List(list),
            ..
        } = &mut expr.kind
        else {
            panic!("expected a function call");
        };
        *n = name.into();
        let first = list[0].clone();
        list.clear();
        list.resize(arity, first);
        let mut ctx = Ctx {
            next_reg: 0,
            column_regs: HashMap::new(),
            program: Vec::new(),
        };
        compile_expr(expr, &mut ctx);
        ctx.program
    }

    fn has_nocase_like(program: &[Instruction]) -> bool {
        program.iter().any(|i| {
            matches!(
                i.opcode,
                Opcode::Map {
                    op: MapOp::Like { nocase: true, .. },
                    ..
                }
            )
        })
    }

    fn call_names(program: &[Instruction]) -> Vec<String> {
        program
            .iter()
            .filter_map(|i| match &i.opcode {
                Opcode::Call { name, .. } => Some(name.to_string()),
                _ => None,
            })
            .collect()
    }

    // codegen_batch_compile_expr_c40c1a63 (`compile_expr`):
    // `name.eq_ignore_ascii_case("ilike") && list.len() == 2`
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_batch_compile_expr_c40c1a63__v1_ilike_with_two_args_is_a_nocase_like() {
        let program = compile_call("ILIKE", 2);
        assert!(has_nocase_like(&program));
        assert!(call_names(&program).is_empty());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_batch_compile_expr_c40c1a63__v2_ilike_with_three_args_is_a_plain_call() {
        let program = compile_call("ilike", 3);
        assert!(!has_nocase_like(&program));
        assert_eq!(call_names(&program).len(), 1);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_batch_compile_expr_c40c1a63__v3_other_name_with_two_args_is_a_plain_call() {
        let program = compile_call("json_extract", 2);
        assert!(!has_nocase_like(&program));
        assert_eq!(call_names(&program).len(), 1);
    }

    fn distinct_item() -> Item {
        Item::Agg(AggFunc::Count, Some(AggArg::Column("x".into())), None, true)
    }

    // codegen_batch_grouped_output_order_c4d07137 (`grouped_output_order`):
    // `num_group_keys == 0 && !has_distinct`
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_batch_grouped_output_order_c4d07137__v1_no_keys_no_distinct_is_identity() {
        assert!(grouped_output_order(&[count_item(), count_item()], 0).is_empty());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_batch_grouped_output_order_c4d07137__v2_keys_are_reordered() {
        let items = [count_item(), Item::Column("k".into())];
        assert_eq!(grouped_output_order(&items, 1), [1, 0]);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_batch_grouped_output_order_c4d07137__v3_a_distinct_count_moves_first() {
        // Merged as [COUNT(DISTINCT x), COUNT(*)]: the select list's
        // COUNT(*) first is merged column 1.
        assert_eq!(
            grouped_output_order(&[count_item(), distinct_item()], 0),
            [1, 0]
        );
    }

    #[test]
    fn count_distinct_is_labelled_and_other_distinct_aggregates_are_refused() {
        let select =
            sql::parse("SELECT region, COUNT(DISTINCT amount) FROM t GROUP BY region").unwrap();
        assert_eq!(
            output_column_names(&select),
            ["region", "COUNT(DISTINCT amount)"]
        );
        for sql_text in [
            "SELECT SUM(DISTINCT amount) FROM t",
            "SELECT COUNT(DISTINCT amount) * 2 FROM t",
        ] {
            assert!(sql::parse(sql_text).is_err(), "{sql_text}");
        }
        // The planner refuses the same shapes on its own, for an AST that
        // did not come through the column parser.
        for sql_text in [
            "SELECT SUM(DISTINCT amount) FROM t",
            "SELECT COUNT(DISTINCT amount) * 2 FROM t",
            "SELECT COUNT(DISTINCT amount), COUNT(DISTINCT region) FROM t",
            "SELECT COUNT(DISTINCT amount) OVER (PARTITION BY region) FROM t",
        ] {
            let crate::parser::row::error::ParseOutcome::Accepted(select) =
                crate::parser::row::error::parse_select(sql_text)
            else {
                panic!("{sql_text} did not parse");
            };
            assert!(
                matches!(compile(&select), Err(PlanError::UnsupportedSelectItem(_))),
                "{sql_text}"
            );
        }
    }

    #[test]
    fn grouped_output_follows_the_select_list_when_an_aggregate_comes_first() {
        // One segment merges chunks directly, 256 pre-aggregate per worker.
        for segments in [1, 256] {
            let n = i64::try_from(segments).unwrap();
            assert_eq!(
                sorted(run_grouped(
                    "SELECT MIN(amount), region FROM t GROUP BY region",
                    segments
                )),
                vec![
                    vec![Value::Int(3), Value::Str("b".into())],
                    vec![Value::Int(5), Value::Str("a".into())],
                ],
            );
            assert_eq!(
                sorted(run_grouped(
                    "SELECT COUNT(*), region, SUM(amount), amount FROM t GROUP BY region, amount",
                    segments
                )),
                sorted(vec![
                    vec![
                        Value::Int(n),
                        Value::Str("a".into()),
                        Value::Int(5 * n),
                        Value::Int(5)
                    ],
                    vec![
                        Value::Int(n),
                        Value::Str("a".into()),
                        Value::Int(20 * n),
                        Value::Int(20)
                    ],
                    vec![
                        Value::Int(n),
                        Value::Str("b".into()),
                        Value::Int(3 * n),
                        Value::Int(3)
                    ],
                    vec![
                        Value::Int(n),
                        Value::Str("b".into()),
                        Value::Int(30 * n),
                        Value::Int(30)
                    ],
                ]),
            );
        }
    }

    #[test]
    fn grouped_output_order_covers_avg_and_aggregate_expressions() {
        assert_eq!(
            sorted(run_grouped(
                "SELECT AVG(amount), SUM(amount) * 2, region FROM t GROUP BY region",
                1
            )),
            vec![
                vec![Value::Float(12.5), Value::Int(50), Value::Str("a".into())],
                vec![Value::Float(16.5), Value::Int(66), Value::Str("b".into())],
            ],
        );
    }

    #[test]
    fn a_grouped_aggregate_expression_is_computed_per_group() {
        // Was NULL for every group: `Combine` read the expression's row
        // count from the key column it had already moved out. An integer
        // times an integer literal is an integer (#623, as DuckDB).
        assert_eq!(
            run_grouped("SELECT region, SUM(amount) * 2 FROM t GROUP BY region", 1),
            vec![
                vec![Value::Str("a".into()), Value::Int(50)],
                vec![Value::Str("b".into()), Value::Int(66)],
            ],
        );
    }

    #[test]
    fn order_by_a_key_listed_after_an_aggregate_sorts_by_that_key() {
        assert_eq!(
            run_grouped(
                "SELECT SUM(amount), region FROM t GROUP BY region ORDER BY region DESC",
                1
            ),
            vec![
                vec![Value::Int(33), Value::Str("b".into())],
                vec![Value::Int(25), Value::Str("a".into())],
            ],
        );
        assert_eq!(
            run_grouped(
                "SELECT MAX(amount) AS m, region FROM t GROUP BY region ORDER BY m LIMIT 1",
                1
            ),
            vec![vec![Value::Int(20), Value::Str("a".into())]],
        );
    }

    // codegen_batch_grouped_output_order_476ef2ae:
    // `next_key != num_group_keys || identity`
    fn count_item() -> Item {
        Item::Agg(AggFunc::Count, None, None, false)
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_batch_grouped_output_order_476ef2ae__v1_aggregate_first_reorders() {
        let items = [count_item(), Item::Column("k".into())];
        assert_eq!(grouped_output_order(&items, 1), vec![1, 0]);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_batch_grouped_output_order_476ef2ae__v2_missing_key_keeps_merged_order() {
        let items = [count_item(), Item::Column("k".into())];
        assert_eq!(grouped_output_order(&items, 2), Vec::<usize>::new());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_batch_grouped_output_order_476ef2ae__v3_keys_first_is_identity() {
        let items = [Item::Column("k".into()), count_item()];
        assert_eq!(grouped_output_order(&items, 1), Vec::<usize>::new());
    }

    #[test]
    fn keys_first_select_lists_keep_an_empty_output_order() {
        for sql_text in [
            "SELECT region, SUM(amount) FROM t GROUP BY region",
            "SELECT SUM(amount) FROM t",
            "SELECT region FROM t",
        ] {
            let program = compile(&sql::parse(sql_text).unwrap()).unwrap();
            assert!(
                program.opcodes().any(|op| matches!(
                    op,
                    Opcode::Combine { output_order, .. } if output_order.is_empty()
                )),
                "{sql_text}"
            );
        }
        let program =
            compile(&sql::parse("SELECT SUM(amount), region FROM t GROUP BY region").unwrap())
                .unwrap();
        assert!(program.opcodes().any(|op| matches!(
            op,
            Opcode::Combine { output_order, .. } if output_order[..] == [1, 0]
        )));
    }

    #[test]
    fn flat_sum_filter_excludes_non_matching_rows() {
        let query = sql::parse("SELECT SUM(amount) FILTER (WHERE amount > 10) FROM t").unwrap();
        let program = compile(&query).unwrap();
        assert_eq!(run_program(&program), vec![vec![Value::Int(50)]]);
    }

    #[test]
    fn count_star_filter_counts_only_matching_rows() {
        let query = sql::parse("SELECT COUNT(*) FILTER (WHERE amount > 10) FROM t").unwrap();
        let program = compile(&query).unwrap();
        assert_eq!(run_program(&program), vec![vec![Value::Int(2)]]);
    }

    #[test]
    fn group_by_sum_filter_masks_per_group() {
        let query = sql::parse(
            "SELECT region, SUM(amount) FILTER (WHERE amount > 10) FROM t GROUP BY region",
        )
        .unwrap();
        let program = compile(&query).unwrap();
        let mut rows = run_program(&program);
        rows.sort_by(|a, b| format!("{:?}", a[0]).cmp(&format!("{:?}", b[0])));
        assert_eq!(
            rows,
            vec![
                vec![Value::Str("a".into()), Value::Int(20)],
                vec![Value::Str("b".into()), Value::Int(30)],
            ]
        );
    }

    #[test]
    fn window_sum_filter_masks_the_running_sum() {
        let query = sql::parse(
            "SELECT SUM(amount) FILTER (WHERE amount > 10) OVER (PARTITION BY region) FROM t",
        )
        .unwrap();
        let program = compile_window(&query).unwrap();
        assert_eq!(
            run_program(&program),
            vec![
                vec![Value::Int(20)],
                vec![Value::Int(20)],
                vec![Value::Int(30)],
                vec![Value::Int(30)],
            ]
        );
    }

    #[test]
    fn filter_is_rejected_on_ranking_window_functions() {
        let query =
            sql::parse("SELECT ROW_NUMBER() FILTER (WHERE amount > 10) OVER (ORDER BY id) FROM t")
                .unwrap();
        assert!(matches!(
            classify_items(&query),
            Err(PlanError::UnsupportedSelectItem(_))
        ));
    }

    // ---------------------------------------------------------------------
    // ADR-0026: late materialization -- a plain projection column not
    // referenced by WHERE is deferred past `Filter` so `Program::
    // predicate_columns`/`projection_only_columns` (and, eventually,
    // `RowGroupSegment`) can decode it only for surviving rows.
    // ---------------------------------------------------------------------

    #[test]
    fn projection_only_column_loads_after_filter_predicate_column_loads_before() {
        let query = sql::parse("SELECT id, region FROM t WHERE amount > 10").unwrap();
        let program = compile(&query).unwrap();
        // `amount` isn't projected at all but is still loaded, pre-Filter.
        assert_eq!(program.predicate_columns(), vec!["amount".to_string()]);
        let mut projected = program.projection_only_columns();
        projected.sort();
        assert_eq!(projected, vec!["id".to_string(), "region".to_string()]);
        // Every loaded column is still accounted for by the union of the
        // two (order-insensitive).
        let mut all = program.columns_to_load();
        all.sort();
        let mut expected = vec!["amount".to_string(), "id".to_string(), "region".to_string()];
        expected.sort();
        assert_eq!(all, expected);
    }

    #[test]
    fn where_referenced_projection_column_stays_pre_filter() {
        let query = sql::parse("SELECT amount FROM t WHERE amount > 10").unwrap();
        let program = compile(&query).unwrap();
        assert_eq!(program.predicate_columns(), vec!["amount".to_string()]);
        assert!(program.projection_only_columns().is_empty());
    }

    #[test]
    fn no_where_clause_leaves_projection_only_columns_empty() {
        let query = sql::parse("SELECT id, region FROM t").unwrap();
        let program = compile(&query).unwrap();
        // No `Filter` at all: the whole program counts as "pre-filter" per
        // `predicate_columns`'s doc comment, and there is nothing to defer.
        assert!(program.projection_only_columns().is_empty());
        let mut predicate = program.predicate_columns();
        predicate.sort();
        assert_eq!(predicate, vec!["id".to_string(), "region".to_string()]);
    }

    fn null_amount_batch() -> crate::vm::batch::Batch {
        crate::vm::batch::Batch::new(5)
            .with_column(
                "id",
                vec![
                    Value::Int(1),
                    Value::Int(2),
                    Value::Int(3),
                    Value::Int(4),
                    Value::Int(5),
                ],
            )
            .with_column(
                "region",
                vec![
                    Value::Str("a".into()),
                    Value::Str("a".into()),
                    Value::Str("b".into()),
                    Value::Str("b".into()),
                    Value::Str("c".into()),
                ],
            )
            .with_column(
                "amount",
                vec![
                    Value::Int(5),
                    Value::Null,
                    Value::Int(30),
                    Value::Null,
                    Value::Int(100),
                ],
            )
    }

    fn run_null_amount_program(program: &Program) -> Vec<Vec<Value>> {
        use crate::vm::engine::{run, InMemorySegment};
        run(&[InMemorySegment(null_amount_batch())], program)
            .unwrap()
            .into_rows()
    }

    #[test]
    fn filter_over_null_predicate_column_excludes_null_rows() {
        // NULL > 10 is NULL (not true), so both NULL-amount rows must be
        // excluded -- only ids 3 and 5 survive.
        let query = sql::parse("SELECT id FROM t WHERE amount > 10").unwrap();
        let program = compile(&query).unwrap();
        let rows = run_null_amount_program(&program);
        assert_eq!(rows, vec![vec![Value::Int(3)], vec![Value::Int(5)]]);
    }

    #[test]
    fn filter_over_unprojected_predicate_column_still_projects_other_columns() {
        // `amount` is filtered on but never selected.
        let query = sql::parse("SELECT id, region FROM t WHERE amount > 10").unwrap();
        let program = compile(&query).unwrap();
        assert_eq!(
            run_null_amount_program(&program),
            vec![
                vec![Value::Int(3), Value::Str("b".into())],
                vec![Value::Int(5), Value::Str("c".into())],
            ]
        );
    }

    #[test]
    fn multi_column_predicate_loads_every_referenced_column_pre_filter() {
        let query = sql::parse("SELECT id FROM t WHERE amount > 10 AND region = 'c'").unwrap();
        let program = compile(&query).unwrap();
        let mut predicate = program.predicate_columns();
        predicate.sort();
        assert_eq!(predicate, vec!["amount".to_string(), "region".to_string()]);
        assert_eq!(program.projection_only_columns(), vec!["id".to_string()]);
        assert_eq!(run_null_amount_program(&program), vec![vec![Value::Int(5)]]);
    }

    #[test]
    fn filter_with_zero_survivors_emits_no_rows() {
        let query = sql::parse("SELECT id, region FROM t WHERE amount > 1000").unwrap();
        let program = compile(&query).unwrap();
        let rows = run_null_amount_program(&program);
        assert!(rows.is_empty());
    }

    #[test]
    fn group_by_query_still_loads_key_and_agg_columns_pre_filter() {
        // Out of scope for the optimization itself (ADR-0026): GROUP BY/
        // aggregate columns keep loading pre-Filter regardless of WHERE
        // reference, and results must stay correct.
        let query =
            sql::parse("SELECT region, SUM(amount) FROM t WHERE amount > 10 GROUP BY region")
                .unwrap();
        let program = compile(&query).unwrap();
        assert!(program.projection_only_columns().is_empty());
        let mut rows = run_null_amount_program(&program);
        rows.sort_by(|a, b| format!("{:?}", a[0]).cmp(&format!("{:?}", b[0])));
        assert_eq!(
            rows,
            vec![
                vec![Value::Str("b".into()), Value::Int(30)],
                vec![Value::Str("c".into()), Value::Int(100)],
            ]
        );
    }
}
