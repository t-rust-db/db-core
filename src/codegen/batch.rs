//! Columnar query planner: [`crate::expr::Query`] to an executable
//! [`Program`] for the batch executor -- what sqlite-rs's `src/codegen/*`
//! does for its VDBE (ADR 0007). Moved here from column-rs's `src/query.rs`
//! verbatim in behavior: nothing in this module ever touched Parquet, so
//! it never belonged in the storage glue.
//!
//! Four entry points, one per query shape the executor distinguishes:
//!
//! - [`compile`] -- flat/`GROUP BY`/`ORDER BY`/`LIMIT` single-table
//!   queries. The output [`Program`] ends in [`Opcode::Finalize`], which
//!   carries the cross-segment merge/sort/limit metadata; the columns to
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
//!   ending in `Emit` + `Finalize` like any other flat program.
//!
//! Plus [`explain`], the `EXPLAIN` plan-tree construction over the same
//! planning decisions, and [`output_column_names`] for result headers.

use crate::expr::{
    AggFunc, BinOp, Expr, JoinKind, OrderBy, Query, SelectItem, WindowFunc, WindowSpec,
};
use crate::types::Literal;
use crate::vm::batch::{AggPart, Instruction, MapOp, Opcode, Program, Value};
use crate::vm::engine::JoinProgram;
use std::collections::HashMap;
use std::fmt;

/// Planning failures: a column that resolves to no table, or a query shape
/// the executor doesn't implement. Storage-level failures (unknown table,
/// unreadable file) are the caller's, not the planner's.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanError {
    UnknownColumn(String),
    UnsupportedSemiJoin(String),
    /// `Right`/`Full`/`Cross` are parseable but only `Inner`/`Left` hash-
    /// join execution exists so far.
    UnsupportedJoinKind(JoinKind),
    /// `SELECT *` (or a mixed `SELECT col, *`) combined with `GROUP BY`, an
    /// aggregate, or a window function -- standard SQL rejects this too,
    /// since there's no well-defined column list to expand `*` into once
    /// the row shape is collapsed/reordered by those clauses.
    StarWithAggregation,
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
        }
    }
}

/// Expand every [`SelectItem::Star`] in `query.columns` into a
/// [`SelectItem::Column`] per entry of `schema` (in `schema`'s order),
/// leaving every other select item untouched -- so `SELECT id, * FROM t`
/// keeps `id` first and expands `*` after it. `schema` is the resolved
/// table's column names; `sql-parser` never sees these (see
/// [`SelectItem::Star`]'s docs), so this is the caller's (the executor
/// with Parquet/table schema access) job to run once, before handing the
/// query to [`compile`]/[`compile_join`]/[`compile_semi_join`]/
/// [`compile_window`].
///
/// Returns [`PlanError::StarWithAggregation`] if `*` is combined with
/// `GROUP BY` or an aggregate/window select item. A query with no `Star`
/// item is returned unchanged (cloned).
pub fn expand_star(query: &Query, schema: &[String]) -> Result<Query> {
    if !query.columns.iter().any(|c| matches!(c, SelectItem::Star)) {
        return Ok(query.clone());
    }
    let has_aggregation = !query.group_by.is_empty()
        || query
            .columns
            .iter()
            .any(|c| matches!(c, SelectItem::Agg(..) | SelectItem::Window(_)));
    if has_aggregation {
        return Err(PlanError::StarWithAggregation);
    }
    let mut columns = Vec::with_capacity(query.columns.len() + schema.len());
    for item in &query.columns {
        match item {
            SelectItem::Star => columns.extend(schema.iter().cloned().map(SelectItem::Column)),
            other => columns.push(other.clone()),
        }
    }
    Ok(Query {
        columns,
        ..query.clone()
    })
}

impl std::error::Error for PlanError {}

pub type Result<T> = std::result::Result<T, PlanError>;

fn map_bin_op(op: BinOp) -> MapOp {
    match op {
        BinOp::Add => MapOp::Add,
        BinOp::Sub => MapOp::Sub,
        BinOp::Mul => MapOp::Mul,
        BinOp::Div => MapOp::Div,
        BinOp::Eq => MapOp::Eq,
        BinOp::Ne => MapOp::Ne,
        BinOp::Lt => MapOp::Lt,
        BinOp::Le => MapOp::Le,
        BinOp::Gt => MapOp::Gt,
        BinOp::Ge => MapOp::Ge,
        BinOp::And => MapOp::And,
        BinOp::Or => MapOp::Or,
        BinOp::Concat => MapOp::Concat,
    }
}

fn literal_value(lit: &Literal) -> Value {
    match lit {
        Literal::Int(v) => Value::Int(*v),
        Literal::Float(v) => Value::Float(*v),
        Literal::Str(v) => Value::Str(v.clone().into()),
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
        self.next_reg += 1;
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

fn compile_expr(expr: &Expr, ctx: &mut Ctx) -> usize {
    match expr {
        Expr::Column(name) => ctx.load_column(name),
        Expr::Literal(lit) => {
            let reg = ctx.alloc();
            ctx.push(Opcode::LoadConst {
                reg,
                value: literal_value(lit),
            });
            reg
        }
        // `EXISTS (SELECT ...)` has no batch-planner counterpart either
        // (db-core#95 implements it for `codegen::row` only) -- compile
        // to the same always-false predicate.
        Expr::InSubquery { .. } | Expr::Exists { .. } => {
            // `compile_semi_join` handles `IN (subquery)` itself and strips
            // it from `where_clause` before ever calling `compile` --
            // reaching this arm means `IN (subquery)` was used via the
            // regular single-table path, which can't run a subquery.
            // Compile to an always-false predicate (no rows) rather than
            // panicking.
            let reg = ctx.alloc();
            ctx.push(Opcode::LoadConst {
                reg,
                value: Value::Bool(false),
            });
            reg
        }
        Expr::BinaryOp(lhs, op, rhs) => {
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
        Expr::Not(inner) => {
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
        Expr::Neg(inner) => {
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
        Expr::IsNull { expr, negated } => {
            let a = compile_expr(expr, ctx);
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
    }
}

/// Compile a flat/`GROUP BY`/`ORDER BY`/`LIMIT` query into a [`Program`]
/// ending in [`Opcode::Finalize`]. Compiled once, reused across every
/// segment.
pub fn compile(query: &Query) -> Program {
    let mut ctx = Ctx {
        next_reg: 0,
        column_regs: HashMap::new(),
        program: Vec::new(),
    };

    // Load every column the group-by keys and select-list aggregates need
    // *before* compiling WHERE/Filter: Filter only shrinks registers that
    // are already live, so anything loaded afterwards would keep the
    // batch's full (pre-filter) length and desync from filtered registers.
    let mut group_by_regs = Vec::new();
    for name in &query.group_by {
        group_by_regs.push(ctx.load_column(name));
    }
    let mut agg_srcs = Vec::new();
    for item in &query.columns {
        match item {
            SelectItem::Agg(_, Some(name)) => {
                agg_srcs.push(ctx.load_column(name));
            }
            // Plain projected columns are emitted (not aggregated), but they
            // must be loaded here for the same reason as the keys above: a
            // column first loaded below the Filter keeps its full pre-filter
            // length while the filtered registers shrink, and Emit then
            // indexes past the end of the short ones. `load_column` memoizes,
            // so the projection code further down reuses these registers
            // instead of emitting a second LoadColumn.
            SelectItem::Column(name) if query.group_by.is_empty() => {
                ctx.load_column(name);
                agg_srcs.push(0);
            }
            _ => agg_srcs.push(0),
        }
    }

    if let Some(where_clause) = &query.where_clause {
        let predicate = compile_expr(where_clause, &mut ctx);
        ctx.push_commented(
            Opcode::Filter { predicate },
            format!("WHERE {}", expr_to_string(where_clause)),
        );
    }

    let mut agg_parts = Vec::new();
    for _ in &query.group_by {
        agg_parts.push(AggPart::GroupKey);
    }

    let mut aggs: Vec<(AggFunc, Option<usize>)> = Vec::new();
    let mut agg_dst = Vec::new();
    let mut emit_regs = group_by_regs.clone();

    for (i, item) in query.columns.iter().enumerate() {
        if let SelectItem::Agg(func, arg) = item {
            let src = arg.as_ref().map(|_| agg_srcs[i]);
            match func {
                AggFunc::Avg => {
                    let sum_dst = ctx.alloc();
                    let count_dst = ctx.alloc();
                    aggs.push((AggFunc::Sum, src));
                    agg_dst.push(sum_dst);
                    aggs.push((AggFunc::Count, src));
                    agg_dst.push(count_dst);
                    agg_parts.push(AggPart::Avg(emit_regs.len(), emit_regs.len() + 1));
                    emit_regs.push(sum_dst);
                    emit_regs.push(count_dst);
                }
                other => {
                    let dst = ctx.alloc();
                    aggs.push((*other, src));
                    agg_dst.push(dst);
                    agg_parts.push(match other {
                        AggFunc::Sum => AggPart::Sum,
                        AggFunc::Count => AggPart::Count,
                        AggFunc::Min => AggPart::Min,
                        AggFunc::Max => AggPart::Max,
                        AggFunc::Avg => unreachable!(),
                    });
                    emit_regs.push(dst);
                }
            }
        } else if let SelectItem::Column(name) = item {
            // A plain column in the SELECT list: if there's no GROUP BY,
            // it isn't loaded/emitted anywhere else yet, so load and emit
            // it directly here. With a GROUP BY, it's expected to already
            // be one of the group-by columns (already in `emit_regs` via
            // `group_by_regs` above) -- SQL requires non-aggregated SELECT
            // columns to be group-by keys, so this doesn't double-emit.
            if query.group_by.is_empty() {
                let reg = ctx.load_column(name);
                emit_regs.push(reg);
            }
        }
    }

    if !aggs.is_empty() || !group_by_regs.is_empty() {
        let comment = if query.group_by.is_empty() {
            "aggregate".to_string()
        } else {
            format!("GROUP BY {}", query.group_by.join(", "))
        };
        ctx.push_commented(
            Opcode::GroupReduce {
                group_by: group_by_regs.into(),
                aggs: aggs.into(),
                agg_dst: agg_dst.into(),
            },
            comment,
        );
    }

    ctx.push_commented(
        Opcode::Emit {
            registers: emit_regs.into(),
        },
        format!("SELECT {}", output_column_names(query).join(", ")),
    );

    let order_by = query
        .order_by
        .as_ref()
        .and_then(|OrderBy { column, descending }| {
            select_output_index(query, column).map(|pos| (pos, *descending))
        });

    ctx.push_commented(
        Opcode::Finalize {
            agg_parts: agg_parts.into(),
            num_group_keys: query.group_by.len(),
            distinct: query.distinct,
            order_by,
            limit: query.limit,
        },
        finalize_comment(query),
    );

    Program::new(ctx.program)
}

fn finalize_comment(query: &Query) -> String {
    let mut parts = Vec::new();
    if !query.group_by.is_empty()
        || query
            .columns
            .iter()
            .any(|c| matches!(c, SelectItem::Agg(..)))
    {
        parts.push("merge partial aggregates".to_string());
    }
    if let Some(OrderBy { column, descending }) = &query.order_by {
        parts.push(format!(
            "ORDER BY {column}{}",
            if *descending { " DESC" } else { "" }
        ));
    }
    if let Some(limit) = query.limit {
        parts.push(format!("LIMIT {limit}"));
    }
    if parts.is_empty() {
        "concatenate segments".to_string()
    } else {
        parts.join("; ")
    }
}

/// Split a (possibly qualified) column name into `(table_prefix, column)`.
pub fn split_qualified(name: &str) -> (Option<&str>, &str) {
    match name.find('.') {
        Some(idx) => (Some(&name[..idx]), &name[idx + 1..]),
        None => (None, name),
    }
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
pub fn compile_join(query: &Query) -> Result<JoinProgram> {
    #[allow(
        clippy::expect_used,
        reason = "dispatch only routes here for queries with a JOIN clause"
    )]
    let join = query
        .joins
        .first()
        .expect("compile_join requires at least one join");
    if !matches!(join.kind, JoinKind::Inner | JoinKind::Left) {
        return Err(PlanError::UnsupportedJoinKind(join.kind));
    }
    let body = compile(query);

    let mut needed: Vec<String> = body.columns_to_load();
    for extra in [&join.left_col, &join.right_col] {
        if !needed.contains(extra) {
            needed.push(extra.clone());
        }
    }

    let mut left_columns = Vec::new();
    let mut right_columns = Vec::new();
    for name in &needed {
        let (prefix, _) = split_qualified(name);
        match prefix {
            None => left_columns.push(name.clone()),
            Some(p) if p == query.from.name() => left_columns.push(name.clone()),
            Some(p) if p == join.table => right_columns.push(name.clone()),
            Some(_) => return Err(PlanError::UnknownColumn(name.clone())),
        }
    }

    let right_key_reg = right_columns
        .iter()
        .position(|n| n == &join.right_col)
        .ok_or_else(|| PlanError::UnknownColumn(join.right_col.clone()))?;
    let build = Program::from_opcodes(
        right_columns
            .iter()
            .enumerate()
            .map(|(reg, name)| Opcode::LoadColumn {
                reg,
                column: name.clone().into(),
            })
            .chain(std::iter::once(Opcode::HashBuild {
                key_cols: vec![right_key_reg].into(),
                payload_cols: (0..right_columns.len()).collect::<Vec<_>>().into(),
                table: 0,
            })),
    );

    let left_key_reg = left_columns
        .iter()
        .position(|n| n == &join.left_col)
        .ok_or_else(|| PlanError::UnknownColumn(join.left_col.clone()))?;
    let payload_dst: Vec<usize> = (0..right_columns.len())
        .map(|i| left_columns.len() + i)
        .collect();
    let join_kind = match join.kind {
        JoinKind::Inner => crate::vm::batch::JoinKind::Inner,
        JoinKind::Left => crate::vm::batch::JoinKind::Left,
        _ => unreachable!("checked at the top of compile_join"),
    };
    let probe = Program::from_opcodes(
        left_columns
            .iter()
            .enumerate()
            .map(|(reg, name)| Opcode::LoadColumn {
                reg,
                column: name.clone().into(),
            })
            .chain(std::iter::once(Opcode::HashProbe {
                key_cols: vec![left_key_reg].into(),
                table: 0,
                payload_dst: payload_dst.clone().into(),
                kind: join_kind,
            })),
    );

    Ok(JoinProgram {
        left_columns,
        right_columns,
        build,
        probe,
        payload_dst,
        body,
    })
}

/// A planned `WHERE col IN (SELECT ...)` semi-join: the caller plans and
/// runs `subquery` via [`compile`] on its own table, collects the allowed
/// key set, filters the main table on `key_column` (see
/// [`crate::vm::engine::semi_filter`]), and runs `body` over the survivors.
/// `body` is the main query compiled with the `IN` clause stripped -- the
/// subquery isn't a VM predicate.
#[derive(Debug, Clone, PartialEq)]
pub struct SemiJoinProgram<'q> {
    pub key_column: &'q str,
    pub subquery: &'q Query,
    pub body: Program,
}

/// Plan a query whose entire `WHERE` clause is `col IN (SELECT ...)`.
/// Combining the `IN` clause with other conditions via `AND`/`OR` isn't
/// supported -- the semi-join must be the whole `WHERE` clause.
pub fn compile_semi_join(query: &Query) -> Result<SemiJoinProgram<'_>> {
    let Some(Expr::InSubquery { expr, subquery }) = &query.where_clause else {
        return Err(PlanError::UnsupportedSemiJoin(
            "WHERE clause must be exactly `col IN (SELECT ...)`".to_string(),
        ));
    };
    let Expr::Column(key_column) = expr.as_ref() else {
        return Err(PlanError::UnsupportedSemiJoin(
            "IN's left-hand side must be a bare column".to_string(),
        ));
    };

    let mut stripped = query.clone();
    stripped.where_clause = None;
    Ok(SemiJoinProgram {
        key_column,
        subquery,
        body: compile(&stripped),
    })
}

/// `crate::expr::WindowFunc` and `crate::vm::batch::WindowFunc` are
/// separate types (same variants) so that `expr` (AST) doesn't depend on
/// `vm` (execution) -- convert at the point the planner hands a spec to
/// the VM.
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

/// Plan a query whose `SELECT` list contains one or more window functions
/// (`ROW_NUMBER`/`RANK`/`DENSE_RANK`, `LAG`/`LEAD`, `FIRST_VALUE`/
/// `LAST_VALUE`, `SUM`/`AVG`/`COUNT OVER`). Window functions need the
/// whole table materialized (partitioning and sorting happen over the
/// entire input), so the caller runs this program over a single in-memory
/// segment. `WHERE` and plain aggregates aren't supported combined with
/// window functions in this minimal implementation -- only plain columns
/// and window items in `SELECT`; an aggregate or `*` item emits NULL.
///
/// `LAST_VALUE`'s default frame (`RANGE UNBOUNDED PRECEDING .. CURRENT
/// ROW`, per the SQL standard when `ORDER BY` is present in `OVER`) makes
/// it return the *current* row's value, not the partition's true last row
/// -- that's implemented literally, ignoring `RANGE` peer-group ties.
pub fn compile_window(query: &Query) -> Program {
    let mut needed: Vec<String> = Vec::new();
    let push_needed = |name: &str, needed: &mut Vec<String>| {
        if !needed.iter().any(|n| n == name) {
            needed.push(name.to_string());
        }
    };
    for item in &query.columns {
        match item {
            SelectItem::Column(name) => push_needed(name, &mut needed),
            SelectItem::Window(spec) => {
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
            SelectItem::Agg(..) | SelectItem::Star => {}
        }
    }

    // `needed[i]` is loaded into register `i`; each `Opcode::Window`
    // writes its result into a fresh register past those.
    #[allow(
        clippy::expect_used,
        reason = "`needed` is built from the same window specs a few lines above"
    )]
    let column_reg = |name: &str| {
        needed
            .iter()
            .position(|n| n == name)
            .expect("needed columns include every window spec's arg/partition_by/order_by column")
    };

    let mut program: Vec<Instruction> = needed
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

    let mut next_reg = needed.len();
    let mut null_reg: Option<usize> = None;
    let mut emit_regs = Vec::with_capacity(query.columns.len());
    for item in &query.columns {
        match item {
            SelectItem::Column(name) => emit_regs.push(column_reg(name)),
            SelectItem::Window(spec) => {
                let dst = next_reg;
                next_reg += 1;
                program.push(Instruction::with_comment(
                    Opcode::Window {
                        func: map_window_func(spec.func),
                        arg: spec.arg.as_deref().map(column_reg),
                        offset: spec.offset,
                        partition_by: spec
                            .partition_by
                            .iter()
                            .map(|p| column_reg(p))
                            .collect::<Vec<_>>()
                            .into(),
                        order_by: spec
                            .order_by
                            .iter()
                            .map(|(o, desc)| (column_reg(o), *desc))
                            .collect::<Vec<_>>()
                            .into(),
                        dst,
                    },
                    format!("r{dst} = {}", window_detail(spec)),
                ));
                emit_regs.push(dst);
            }
            SelectItem::Agg(..) | SelectItem::Star => {
                let reg = *null_reg.get_or_insert_with(|| {
                    let reg = next_reg;
                    next_reg += 1;
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
        format!("SELECT {}", output_column_names(query).join(", ")),
    ));

    let order_by = query
        .order_by
        .as_ref()
        .and_then(|OrderBy { column, descending }| {
            select_output_index(query, column).map(|pos| (pos, *descending))
        });
    program.push(Instruction::with_comment(
        Opcode::Finalize {
            agg_parts: Vec::new().into(),
            num_group_keys: 0,
            distinct: query.distinct,
            order_by,
            limit: query.limit,
        },
        finalize_comment(query),
    ));

    Program::new(program)
}

/// Resolves an `ORDER BY` reference to its position in the `SELECT`
/// list, matching against each item's rendered output label -- not just
/// [`SelectItem::Column`] -- so `ORDER BY COUNT(x)` resolves to a
/// `SELECT COUNT(x)` item the same way `ORDER BY x` resolves to `SELECT
/// x` (#131: `parser::column`'s `ORDER BY` lowering renders an
/// aggregate reference through the identical [`select_item_label`]
/// format for exactly this reason).
fn select_output_index(query: &Query, column: &str) -> Option<usize> {
    query
        .columns
        .iter()
        .position(|item| select_item_label(item) == column)
}

/// Derive each `SELECT`-list item's output column header (e.g. `SUM(amount)`,
/// `ROW_NUMBER()`) -- shared by the interpreter and the AOT emitter, which
/// both need the same naming for a compiled query's results.
pub fn output_column_names(query: &Query) -> Vec<String> {
    query.columns.iter().map(select_item_label).collect()
}

// ---------------------------------------------------------------------
// EXPLAIN
// ---------------------------------------------------------------------

/// One node in an [`explain`] plan tree: `parent == id` marks the root.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanNode {
    pub id: u32,
    pub parent: u32,
    pub detail: String,
}

/// What `EXPLAIN`'s `SCAN` node reports about a table -- the only thing
/// the planner needs from storage, supplied by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableStats {
    pub row_groups: usize,
    pub rows: i64,
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
        self.next_id += 1;
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

/// Build a human-readable execution plan for `query` without running it
/// (#99). Mirrors the executor's dispatch (semi-join, join, windowed, or
/// plain single-table) over the same planning decisions [`compile`] makes;
/// `stats` supplies each referenced table's `SCAN` detail.
pub fn explain(query: &Query, stats: &dyn Fn(&str) -> TableStats) -> Vec<PlanNode> {
    let mut b = PlanBuilder::new("QUERY PLAN");

    let has_window = query
        .columns
        .iter()
        .any(|c| matches!(c, SelectItem::Window(_)));
    let is_semi_join = matches!(query.where_clause, Some(Expr::InSubquery { .. }));
    let join = query.joins.first();

    // Semi-joins compile with `where_clause` stripped, mirroring
    // `compile_semi_join` (the `IN` subquery isn't a VM predicate).
    let program = if has_window {
        None
    } else if is_semi_join {
        let mut stripped = query.clone();
        stripped.where_clause = None;
        Some(compile(&stripped))
    } else {
        Some(compile(query))
    };
    let columns_to_load = program.as_ref().map(Program::columns_to_load);

    let mut main_cols: Vec<String> = match (&columns_to_load, join) {
        (Some(cols), Some(_)) => cols
            .iter()
            .filter(|n| split_qualified(n).0.is_none_or(|t| t == query.from.name()))
            .cloned()
            .collect(),
        (Some(cols), None) => cols.clone(),
        (None, _) => referenced_columns(query),
    };
    if let Some(j) = join {
        push_unique(&mut main_cols, j.left_col.clone());
    }
    if let Some(Expr::InSubquery { expr, .. }) = &query.where_clause {
        if let Expr::Column(col_name) = expr.as_ref() {
            push_unique(&mut main_cols, col_name.clone());
        }
    }
    let scan = b.push(0, scan_detail(query.from.name(), stats(query.from.name())));
    if !main_cols.is_empty() {
        b.push(scan, format!("LOAD COLUMNS: {}", main_cols.join(", ")));
    }

    if is_semi_join {
        if let Some(Expr::InSubquery { expr, subquery }) = &query.where_clause {
            let sub_scan = b.push(
                0,
                scan_detail(subquery.from.name(), stats(subquery.from.name())),
            );
            let sub_cols = referenced_columns(subquery);
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
                    subquery.from.name()
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
        let kind = match join.kind {
            JoinKind::Inner => "HASH JOIN",
            JoinKind::Left => "LEFT HASH JOIN",
            JoinKind::Right => "RIGHT HASH JOIN",
            JoinKind::Full => "FULL HASH JOIN",
            JoinKind::Cross => "CROSS JOIN",
        };
        b.push(0, format!("{kind}: {} = {}", join.left_col, join.right_col));
    }

    if has_window {
        for item in &query.columns {
            if let SelectItem::Window(spec) = item {
                b.push(0, format!("WINDOW: {}", window_detail(spec)));
            }
        }
    } else if let Some(program) = &program {
        if program
            .opcodes()
            .any(|op| matches!(op, Opcode::Filter { .. }))
        {
            if let Some(where_clause) = &query.where_clause {
                b.push(0, format!("FILTER: {}", expr_to_string(where_clause)));
            }
        }
        if !query.group_by.is_empty() {
            let group_node = b.push(0, format!("GROUP BY: {}", query.group_by.join(", ")));
            for item in &query.columns {
                if matches!(item, SelectItem::Agg(..)) {
                    b.push(
                        group_node,
                        format!("AGGREGATE: {}", select_item_label(item)),
                    );
                }
            }
        } else {
            for item in &query.columns {
                if matches!(item, SelectItem::Agg(..)) {
                    b.push(0, format!("AGGREGATE: {}", select_item_label(item)));
                }
            }
        }
        // DISTINCT runs as a post-Finalize dedup pass, after GROUP BY's
        // hash-aggregate merge and before ORDER BY/LIMIT (see
        // `compile`'s `distinct` handling) -- the plan reflects that order.
        if query.distinct {
            b.push(0, "DISTINCT".to_string());
        }
    }

    if let Some(OrderBy { column, descending }) = &query.order_by {
        b.push(
            0,
            format!(
                "ORDER BY: {column}{}",
                if *descending { " DESC" } else { "" }
            ),
        );
    }
    if let Some(limit) = query.limit {
        b.push(0, format!("LIMIT: {limit}"));
    }

    let emit_labels: Vec<String> = query.columns.iter().map(select_item_label).collect();
    b.push(0, format!("EMIT: {}", emit_labels.join(", ")));

    b.finish()
}

// ---------------------------------------------------------------------
// EXPLAIN (bare form -- opcode listing)
// ---------------------------------------------------------------------

/// One row of a bare `EXPLAIN`'s opcode listing (#55): `addr | opcode |
/// operands | comment`, mirroring sqlite-rs's own bare-`EXPLAIN` table as
/// far as the batch executor's typed operands allow (ADR 0007).
#[derive(Debug, Clone, PartialEq)]
pub struct OpcodeRow {
    pub addr: usize,
    pub opcode: &'static str,
    pub operands: String,
    pub comment: String,
    /// Whether this row is the [`Opcode::Finalize`] barrier: the boundary
    /// between the parallel per-segment phase and the sequential
    /// cross-segment merge phase (ADR 0007).
    pub is_finalize: bool,
}

/// One named program in a bare `EXPLAIN` listing. A flat query has a
/// single section; a join has `build`/`probe`/`body` ([`JoinProgram`]); a
/// semi-join has a single `body` section (its subquery isn't opcode-driven
/// -- see [`compile_semi_join`]).
#[derive(Debug, Clone, PartialEq)]
pub struct OpcodeSection {
    pub label: String,
    pub rows: Vec<OpcodeRow>,
}

fn render_program(program: &Program) -> Vec<OpcodeRow> {
    program
        .instructions
        .iter()
        .enumerate()
        .map(|(addr, instr)| OpcodeRow {
            addr,
            opcode: instr.opcode.name(),
            operands: render_operands(&instr.opcode),
            comment: instr.comment.clone().unwrap_or_default(),
            is_finalize: matches!(instr.opcode, Opcode::Finalize { .. }),
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
        Opcode::Emit { registers } => format!("registers={registers:?}"),
        Opcode::NextSegment { loop_start } => format!("loop_start={loop_start}"),
        Opcode::Halt => String::new(),
        Opcode::Finalize {
            agg_parts,
            num_group_keys,
            distinct,
            order_by,
            limit,
        } => format!(
            "agg_parts={agg_parts:?} num_group_keys={num_group_keys} distinct={distinct} order_by={order_by:?} limit={limit:?}"
        ),
    }
}

/// Build a bare `EXPLAIN`'s opcode listing for `query` (#55): the compiled
/// [`Program`]'s instructions, one section per phase the executor actually
/// runs -- mirrors [`explain`]'s shape dispatch (semi-join, join, windowed,
/// or plain single-table) but over the real compiled opcodes instead of a
/// hand-built plan tree.
pub fn explain_opcodes(query: &Query) -> Result<Vec<OpcodeSection>> {
    let has_window = query
        .columns
        .iter()
        .any(|c| matches!(c, SelectItem::Window(_)));

    if matches!(query.where_clause, Some(Expr::InSubquery { .. })) {
        let semi = compile_semi_join(query)?;
        Ok(vec![OpcodeSection {
            label: format!(
                "SEMI JOIN body ({} IN (SELECT ... FROM {}))",
                semi.key_column,
                semi.subquery.from.name()
            ),
            rows: render_program(&semi.body),
        }])
    } else if !query.joins.is_empty() {
        let join = compile_join(query)?;
        Ok(vec![
            OpcodeSection {
                label: format!("JOIN build ({})", query.joins[0].table),
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
            rows: render_program(&compile_window(query)),
        }])
    } else {
        Ok(vec![OpcodeSection {
            label: "body".to_string(),
            rows: render_program(&compile(query)),
        }])
    }
}

fn scan_detail(table: &str, stats: TableStats) -> String {
    let groups = stats.row_groups;
    format!(
        "SCAN {table} ({groups} row group{}, ~{} rows)",
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
/// `COUNT(*)`, or `ROW_NUMBER()`.
fn select_item_label(item: &SelectItem) -> String {
    match item {
        SelectItem::Column(name) => name.clone(),
        SelectItem::Star => "*".to_string(),
        SelectItem::Agg(func, arg) => match arg {
            Some(col) => format!("{}({col})", agg_func_name(*func)),
            None => format!("{}(*)", agg_func_name(*func)),
        },
        SelectItem::Window(spec) => format!("{}()", window_func_name(spec.func)),
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

fn literal_to_string(lit: &Literal) -> String {
    match lit {
        Literal::Int(v) => v.to_string(),
        Literal::Float(v) => v.to_string(),
        Literal::Str(v) => format!("'{v}'"),
    }
}

fn bin_op_str(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Eq => "=",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::And => "AND",
        BinOp::Or => "OR",
        BinOp::Concat => "||",
    }
}

/// Renders an `Expr` back to SQL-ish text for plan details, e.g.
/// `amount > 100`.
fn expr_to_string(expr: &Expr) -> String {
    match expr {
        Expr::Column(name) => name.clone(),
        Expr::Literal(lit) => literal_to_string(lit),
        Expr::BinaryOp(lhs, op, rhs) => format!(
            "{} {} {}",
            expr_to_string(lhs),
            bin_op_str(*op),
            expr_to_string(rhs)
        ),
        Expr::InSubquery { expr, subquery } => format!(
            "{} IN (SELECT ... FROM {})",
            expr_to_string(expr),
            subquery.from.name()
        ),
        Expr::Exists { subquery, negated } => format!(
            "{}EXISTS (SELECT ... FROM {})",
            if *negated { "NOT " } else { "" },
            subquery.from.name()
        ),
        Expr::Not(inner) => format!("NOT {}", expr_to_string(inner)),
        Expr::Neg(inner) => format!("-{}", expr_to_string(inner)),
        Expr::IsNull { expr, negated } => format!(
            "{} IS {}NULL",
            expr_to_string(expr),
            if *negated { "NOT " } else { "" }
        ),
    }
}

fn push_unique(out: &mut Vec<String>, name: String) {
    if !out.contains(&name) {
        out.push(name);
    }
}

fn collect_expr_columns(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Column(name) => push_unique(out, name.clone()),
        Expr::Literal(_) => {}
        Expr::BinaryOp(lhs, _, rhs) => {
            collect_expr_columns(lhs, out);
            collect_expr_columns(rhs, out);
        }
        Expr::InSubquery { expr, .. } => collect_expr_columns(expr, out),
        Expr::Exists { .. } => {}
        Expr::Not(inner) => collect_expr_columns(inner, out),
        Expr::Neg(inner) => collect_expr_columns(inner, out),
        Expr::IsNull { expr, .. } => collect_expr_columns(expr, out),
    }
}

/// Every column name `query` references, in first-seen order (used for the
/// `EXPLAIN` `LOAD COLUMNS` detail on paths that don't go through
/// [`compile`], namely windowed queries).
fn referenced_columns(query: &Query) -> Vec<String> {
    let mut out = Vec::new();
    for name in &query.group_by {
        push_unique(&mut out, name.clone());
    }
    for item in &query.columns {
        match item {
            SelectItem::Column(name) => push_unique(&mut out, name.clone()),
            SelectItem::Star => {}
            SelectItem::Agg(_, Some(name)) => push_unique(&mut out, name.clone()),
            SelectItem::Agg(_, None) => {}
            SelectItem::Window(spec) => {
                if let Some(arg) = &spec.arg {
                    push_unique(&mut out, arg.clone());
                }
                for name in &spec.partition_by {
                    push_unique(&mut out, name.clone());
                }
                for (name, _) in &spec.order_by {
                    push_unique(&mut out, name.clone());
                }
            }
        }
    }
    if let Some(where_clause) = &query.where_clause {
        collect_expr_columns(where_clause, &mut out);
    }
    for join in &query.joins {
        push_unique(&mut out, join.left_col.clone());
        push_unique(&mut out, join.right_col.clone());
    }
    if let Some(order_by) = &query.order_by {
        // #131: an `ORDER BY` referencing a SELECT-list aggregate carries
        // that aggregate's rendered label (e.g. `COUNT(x)`), not a real
        // column name -- its underlying column, if any, is already
        // covered above via that item's own `SelectItem::Agg` arm, so
        // only push here when the label isn't itself a `SELECT`-list
        // item (i.e. it's a genuine bare-column reference).
        let is_select_item_label = query
            .columns
            .iter()
            .any(|item| select_item_label(item) == order_by.column);
        if !is_select_item_label {
            push_unique(&mut out, order_by.column.clone());
        }
    }
    out
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
    use crate::parser as sql;
    use crate::vm::engine::bounded_scan_limit;

    #[test]
    fn bounded_scan_limit_accepts_bare_limit() {
        let program = compile(&sql::parse("SELECT id FROM t LIMIT 10").unwrap());
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
            let program = compile(&sql::parse(q).unwrap());
            assert_eq!(bounded_scan_limit(&program), None, "{q}");
        }
    }

    #[test]
    fn compile_where_and_group_by_builds_expected_program_shape() {
        let query =
            sql::parse("SELECT region, SUM(amount) FROM t WHERE amount > 10 GROUP BY region")
                .unwrap();
        let program = compile(&query);
        let columns = program.columns_to_load();
        assert!(columns.contains(&"region".to_string()));
        assert!(columns.contains(&"amount".to_string()));
        assert!(matches!(
            program.opcodes().last(),
            Some(Opcode::Finalize {
                num_group_keys: 1,
                ..
            })
        ));
        let (body, _) = program.split_finalize();
        assert!(matches!(body.last(), Some(Opcode::Emit { .. })));
        assert!(body
            .iter()
            .any(|op| matches!(op, Opcode::GroupReduce { .. })));
        assert!(body.iter().any(|op| matches!(op, Opcode::Filter { .. })));
    }

    #[test]
    fn compile_distinct_sets_finalize_flag_without_group_reduce() {
        let query = sql::parse("SELECT DISTINCT a, b FROM t").unwrap();
        let program = compile(&query);
        assert!(matches!(
            program.opcodes().last(),
            Some(Opcode::Finalize {
                distinct: true,
                num_group_keys: 0,
                ..
            })
        ));
        let (body, _) = program.split_finalize();
        assert!(!body
            .iter()
            .any(|op| matches!(op, Opcode::GroupReduce { .. })));
    }

    #[test]
    fn compile_distinct_with_group_by_sets_both_finalize_flag_and_group_reduce() {
        let query = sql::parse("SELECT DISTINCT region, SUM(amount) FROM t GROUP BY region")
            .expect("DISTINCT + GROUP BY should parse (rewritten as a post-aggregate dedup)");
        let program = compile(&query);
        assert!(matches!(
            program.opcodes().last(),
            Some(Opcode::Finalize {
                distinct: true,
                num_group_keys: 1,
                ..
            })
        ));
        let (body, _) = program.split_finalize();
        assert!(body
            .iter()
            .any(|op| matches!(op, Opcode::GroupReduce { .. })));
    }

    #[test]
    fn compile_encodes_order_by_and_limit_in_finalize_and_comments_instructions() {
        let query = sql::parse("SELECT id, val FROM t ORDER BY val DESC LIMIT 5").unwrap();
        let program = compile(&query);
        let fin = program.instructions.last().unwrap();
        assert_eq!(
            fin.opcode,
            Opcode::Finalize {
                agg_parts: Vec::new().into(),
                num_group_keys: 0,
                distinct: false,
                order_by: Some((1, true)),
                limit: Some(5),
            }
        );
        assert_eq!(fin.comment.as_deref(), Some("ORDER BY val DESC; LIMIT 5"));
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
        let program = compile(&query);
        let fin = program.instructions.last().unwrap();
        assert_eq!(
            fin.opcode,
            Opcode::Finalize {
                agg_parts: vec![AggPart::GroupKey, AggPart::Count, AggPart::Sum].into(),
                num_group_keys: 1,
                distinct: false,
                order_by: Some((1, true)),
                limit: None,
            }
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__batch_355__v1_agg_without_group_by_emits_group_reduce() {
        let query = sql::parse("SELECT SUM(amount) FROM t").unwrap();
        let program = compile(&query);
        let (body, _) = program.split_finalize();
        assert!(body
            .iter()
            .any(|op| matches!(op, Opcode::GroupReduce { .. })));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__batch_355__v2_group_by_without_agg_emits_group_reduce() {
        let query = sql::parse("SELECT region FROM t GROUP BY region").unwrap();
        let program = compile(&query);
        let (body, _) = program.split_finalize();
        assert!(body
            .iter()
            .any(|op| matches!(op, Opcode::GroupReduce { .. })));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__batch_355__v3_no_agg_no_group_by_omits_group_reduce() {
        let query = sql::parse("SELECT id FROM t").unwrap();
        let program = compile(&query);
        let (body, _) = program.split_finalize();
        assert!(!body
            .iter()
            .any(|op| matches!(op, Opcode::GroupReduce { .. })));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__batch_401__v1_group_by_without_agg_column_merges_partial_aggregates() {
        let query = sql::parse("SELECT region FROM t GROUP BY region").unwrap();
        let program = compile(&query);
        let fin = program.instructions.last().unwrap();
        assert_eq!(fin.comment.as_deref(), Some("merge partial aggregates"));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__batch_401__v2_agg_column_without_group_by_merges_partial_aggregates() {
        let query = sql::parse("SELECT SUM(amount) FROM t").unwrap();
        let program = compile(&query);
        let fin = program.instructions.last().unwrap();
        assert_eq!(fin.comment.as_deref(), Some("merge partial aggregates"));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__batch_401__v3_no_group_by_no_agg_column_concatenates_segments() {
        let query = sql::parse("SELECT id FROM t").unwrap();
        let program = compile(&query);
        let fin = program.instructions.last().unwrap();
        assert_eq!(fin.comment.as_deref(), Some("concatenate segments"));
    }

    #[test]
    fn compile_join_splits_columns_by_table_and_builds_both_programs() {
        let query = sql::parse(
            "SELECT orders.id, regions.budget FROM orders JOIN regions ON orders.region_key = regions.rkey",
        )
        .unwrap();
        let plan = compile_join(&query).unwrap();
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
            Some(Opcode::Finalize { .. })
        ));

        let bad = sql::parse("SELECT a.id FROM a JOIN b ON a.id = c.id").unwrap();
        assert_eq!(
            compile_join(&bad),
            Err(PlanError::UnknownColumn("c.id".into()))
        );
    }

    #[test]
    fn compile_semi_join_strips_the_in_clause_from_the_body() {
        let query =
            sql::parse("SELECT id FROM orders WHERE region_key IN (SELECT rkey FROM regions)")
                .unwrap();
        let plan = compile_semi_join(&query).unwrap();
        assert_eq!(plan.key_column, "region_key");
        assert_eq!(plan.subquery.from.name(), "regions");
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
        // Constructed directly rather than via `sql::parse`: window
        // functions (`OVER`/`FILTER`) aren't parseable through the shared
        // `parser::row` grammar this crate unified on (#57) -- tracked as
        // follow-up (extend `row`'s grammar, then give `compile_window` an
        // `ast::Select` input too). `compile_window` itself is untouched.
        let query = Query {
            columns: vec![
                SelectItem::Column("id".into()),
                SelectItem::Window(WindowSpec {
                    func: WindowFunc::RowNumber,
                    arg: None,
                    offset: None,
                    partition_by: vec!["region_key".into()],
                    order_by: vec![("id".into(), false)],
                }),
                SelectItem::Column("region_key".into()),
            ],
            from: "orders".into(),
            joins: vec![],
            where_clause: None,
            distinct: false,
            group_by: vec![],
            having: None,
            order_by: Some(OrderBy {
                column: "id".into(),
                descending: false,
            }),
            limit: None,
            offset: None,
        };
        let program = compile_window(&query);
        assert_eq!(program.columns_to_load(), vec!["id", "region_key"]);
        let (body, fin) = program.split_finalize();
        assert!(matches!(
            fin,
            Some(Opcode::Finalize {
                order_by: Some((0, false)),
                ..
            })
        ));
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
        }
    }

    #[test]
    fn explain_plain_filter_group_by_aggregate() {
        let query = sql::parse("SELECT region, SUM(amount), COUNT(*) FROM production WHERE id > 1000 GROUP BY region ORDER BY region").unwrap();
        let nodes = explain(&query, &stats);

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
        let nodes = explain(&query, &stats);
        assert!(details(&nodes).contains(&"LOAD COLUMNS: orders.id, orders.region_key"));
        assert!(details(&nodes).contains(&"LOAD COLUMNS: regions.budget, regions.rkey"));
        assert!(details(&nodes).contains(&"HASH JOIN: orders.region_key = regions.rkey"));

        let query = sql::parse(
            "SELECT id FROM orders WHERE region_key IN (SELECT rkey FROM regions) ORDER BY id",
        )
        .unwrap();
        let nodes = explain(&query, &stats);
        assert!(details(&nodes).contains(&"SEMI JOIN: region_key IN (SELECT rkey FROM regions)"));
        assert!(details(&nodes).contains(&"LOAD COLUMNS: id, region_key"));
        assert!(details(&nodes).contains(&"LOAD COLUMNS: rkey"));
        assert!(!details(&nodes).iter().any(|d| d.starts_with("FILTER")));

        // Constructed directly: window functions aren't parseable through
        // the shared `parser::row` grammar (#57 follow-up).
        let query = Query {
            columns: vec![
                SelectItem::Column("id".into()),
                SelectItem::Column("region_key".into()),
                SelectItem::Window(WindowSpec {
                    func: WindowFunc::RowNumber,
                    arg: None,
                    offset: None,
                    partition_by: vec!["region_key".into()],
                    order_by: vec![("id".into(), false)],
                }),
            ],
            from: "orders".into(),
            joins: vec![],
            where_clause: None,
            distinct: false,
            group_by: vec![],
            having: None,
            order_by: Some(OrderBy {
                column: "id".into(),
                descending: false,
            }),
            limit: None,
            offset: None,
        };
        let nodes = explain(&query, &stats);
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
        let nodes = explain(&query, &stats);
        assert!(details(&nodes).contains(&"DISTINCT"));
        assert_eq!(nodes.last().unwrap().detail, "EMIT: region");
    }

    #[test]
    fn explain_shows_distinct_after_group_by_and_aggregate() {
        let query =
            sql::parse("SELECT DISTINCT region, SUM(amount) FROM production GROUP BY region")
                .unwrap();
        let nodes = explain(&query, &stats);
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
        let nodes = explain(&query, &stats);
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
        let program = compile(&query);
        let sections = explain_opcodes(&query).unwrap();

        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].label, "body");
        assert_eq!(
            opcodes(&sections[0].rows),
            program.opcodes().map(Opcode::name).collect::<Vec<_>>()
        );
        let finalize_row = sections[0].rows.iter().find(|r| r.is_finalize).unwrap();
        assert_eq!(finalize_row.opcode, "Finalize");
        assert!(sections[0].rows.iter().filter(|r| r.is_finalize).count() == 1);
    }

    #[test]
    fn explain_opcodes_order_by_limit_matches_compiled_program() {
        let query = sql::parse("SELECT id FROM t ORDER BY id LIMIT 5").unwrap();
        let program = compile(&query);
        let sections = explain_opcodes(&query).unwrap();

        assert_eq!(sections.len(), 1);
        assert_eq!(
            opcodes(&sections[0].rows),
            program.opcodes().map(Opcode::name).collect::<Vec<_>>()
        );
        let finalize_row = sections[0].rows.iter().find(|r| r.is_finalize).unwrap();
        assert!(finalize_row.operands.contains("limit=Some(5)"));
    }

    #[test]
    fn explain_opcodes_join_lists_build_probe_and_body() {
        let query = sql::parse(
            "SELECT orders.id, regions.budget FROM orders JOIN regions ON orders.region_key = regions.rkey",
        )
        .unwrap();
        let join = compile_join(&query).unwrap();
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
        // Constructed directly: window functions aren't parseable through
        // the shared `parser::row` grammar this crate unified on (#57
        // follow-up: #67).
        let query = Query {
            columns: vec![
                SelectItem::Column("id".into()),
                SelectItem::Column("region_key".into()),
                SelectItem::Window(WindowSpec {
                    func: WindowFunc::RowNumber,
                    arg: None,
                    offset: None,
                    partition_by: vec!["region_key".into()],
                    order_by: vec![("id".into(), false)],
                }),
            ],
            from: "orders".into(),
            joins: vec![],
            where_clause: None,
            distinct: false,
            group_by: vec![],
            having: None,
            order_by: None,
            limit: None,
            offset: None,
        };
        let program = compile_window(&query);
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
            expanded.columns,
            vec![
                SelectItem::Column("id".to_string()),
                SelectItem::Column("name".to_string()),
                SelectItem::Column("amount".to_string()),
            ]
        );
    }

    #[test]
    fn expand_star_keeps_mixed_columns_in_order() {
        let query = sql::parse("SELECT id, * FROM t").unwrap();
        let schema = vec!["id".to_string(), "name".to_string()];
        let expanded = expand_star(&query, &schema).unwrap();
        assert_eq!(
            expanded.columns,
            vec![
                SelectItem::Column("id".to_string()),
                SelectItem::Column("id".to_string()),
                SelectItem::Column("name".to_string()),
            ]
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
        // Constructed directly: window functions aren't parseable through
        // the shared `parser::row` grammar (#57 follow-up).
        let query = Query {
            columns: vec![
                SelectItem::Star,
                SelectItem::Window(WindowSpec {
                    func: WindowFunc::RowNumber,
                    arg: None,
                    offset: None,
                    partition_by: vec![],
                    order_by: vec![("id".into(), false)],
                }),
            ],
            from: "t".into(),
            joins: vec![],
            where_clause: None,
            distinct: false,
            group_by: vec![],
            having: None,
            order_by: None,
            limit: None,
            offset: None,
        };
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
        let program = compile(&expanded);
        assert_eq!(program.columns_to_load(), vec!["id", "name"]);
    }
}
