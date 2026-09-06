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

use crate::parser::ast::{
    BinaryOp as AstBinOp, Distinctness, Expr as AstExpr, ExprKind, FromClause as AstFromClause,
    FunctionArgs, JoinConstraint, JoinOp, Literal as AstLiteral, ResultColumn, Select,
    TableRefKind,
};
use crate::vm::batch::{AggFunc, AggPart, Instruction, MapOp, Opcode, Program, Value};
use crate::vm::engine::JoinProgram;
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
    Agg(AggFunc, Option<String>),
    Window(WindowSpec),
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

fn agg_arg(_expr: &AstExpr, agg: AggFunc, args: &FunctionArgs) -> Result<Option<String>> {
    match args {
        FunctionArgs::Star => {
            if agg != AggFunc::Count {
                return Err(PlanError::UnsupportedSelectItem(
                    "only COUNT supports (*)".into(),
                ));
            }
            Ok(None)
        }
        FunctionArgs::List(list) => match list.as_slice() {
            [one] => expr_column_name(one).map(Some).ok_or_else(|| {
                PlanError::UnsupportedSelectItem("expected a column reference".into())
            }),
            _ => Err(PlanError::UnsupportedSelectItem(
                "an aggregate takes exactly one column or *".into(),
            )),
        },
    }
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
    })
}

fn classify_item(col: &ResultColumn) -> Result<Item> {
    match col {
        ResultColumn::Star => Ok(Item::Star),
        ResultColumn::TableStar { .. } => Err(PlanError::UnsupportedSelectItem(
            "table.* is not supported".into(),
        )),
        ResultColumn::Expr {
            expr: _,
            alias: Some(_),
        } => Err(PlanError::UnsupportedSelectItem(
            "column alias (AS) is not supported".into(),
        )),
        ResultColumn::Expr { expr, alias: None } => match &expr.kind {
            ExprKind::Column { .. } => expr_column_name(expr).map(Item::Column).ok_or_else(|| {
                PlanError::UnsupportedSelectItem("expected a column reference".into())
            }),
            ExprKind::FunctionCall {
                name,
                distinct: _,
                args,
                over: Some(window_def),
            } => Ok(Item::Window(window_spec(name, args, window_def)?)),
            ExprKind::FunctionCall {
                name,
                distinct: _,
                args,
                over: None,
            } => {
                let agg = AggFunc::from_name(name).ok_or_else(|| {
                    PlanError::UnsupportedSelectItem(format!("unknown function {name}"))
                })?;
                let arg = agg_arg(expr, agg, args)?;
                Ok(Item::Agg(agg, arg))
            }
            _ => Err(PlanError::UnsupportedSelectItem(
                "unsupported SELECT expression".into(),
            )),
        },
    }
}

fn classify_items(select: &Select) -> Result<Vec<Item>> {
    select.columns.iter().map(classify_item).collect()
}

/// The table this `FROM` clause is scanned/joined against: a real table's
/// name, or a `FROM`-subquery's mandatory alias.
fn table_name(from: &AstFromClause) -> &str {
    match &from.first.kind {
        TableRefKind::Name(name) => name,
        TableRefKind::Subquery(_) => from.first.alias.as_deref().unwrap_or(""),
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
}

fn extract_joins(from: &AstFromClause) -> Result<Vec<JoinStep>> {
    from.joins
        .iter()
        .map(|j| {
            let TableRefKind::Name(table) = &j.table.kind else {
                return Err(PlanError::UnknownColumn(
                    "subquery in JOIN is not supported by the batch planner".into(),
                ));
            };
            let (left_col, right_col) = match &j.constraint {
                Some(JoinConstraint::On(expr)) => extract_equi_join(expr)
                    .ok_or_else(|| PlanError::UnknownColumn("JOIN ON must be col = col".into()))?,
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

fn select_limit(select: &Select) -> Option<usize> {
    let limit = select.limit.as_ref()?;
    match &limit.limit.kind {
        ExprKind::Literal(AstLiteral::Integer(n)) if *n >= 0 => Some(*n as usize),
        _ => None,
    }
}

/// Lowers an aggregate `FunctionCall` (`COUNT(x)`, `COUNT(*)`, ...) to
/// the same output label [`select_item_label`] would render for it (via
/// [`AggFunc::name`]) -- used to resolve an `ORDER BY` reference to a
/// `SELECT`-list aggregate (#131) against that same label.
fn aggregate_call_label(name: &str, args: &FunctionArgs) -> Option<String> {
    let agg = AggFunc::from_name(name)?;
    match args {
        FunctionArgs::Star if agg == AggFunc::Count => Some(format!("{}(*)", agg.name())),
        FunctionArgs::Star => None,
        FunctionArgs::List(list) => match list.as_slice() {
            [one] => expr_column_name(one).map(|col| format!("{}({col})", agg.name())),
            _ => None,
        },
    }
}

/// The single `ORDER BY` term's resolved output-column reference (a bare
/// column name or a `SELECT`-list aggregate's rendered label), and whether
/// it's descending. `select.order_by` has at most one term once `select`
/// has passed `parser::column`'s validator.
fn select_order_by(select: &Select) -> Option<(String, bool)> {
    let term = select.order_by.first()?;
    let column = match &term.expr.kind {
        ExprKind::Column { .. } => expr_column_name(&term.expr)?,
        ExprKind::FunctionCall {
            name,
            args,
            over: None,
            ..
        } => aggregate_call_label(name, args)?,
        _ => return None,
    };
    Some((column, term.desc.unwrap_or(false)))
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
/// Returns [`PlanError::StarWithAggregation`] if `*` is combined with
/// `GROUP BY` or an aggregate/window select item. A query with no `Star`
/// item is returned unchanged (cloned).
pub fn expand_star(select: &Select, schema: &[String]) -> Result<Select> {
    let items = classify_items(select)?;
    if !items.iter().any(|c| matches!(c, Item::Star)) {
        return Ok(select.clone());
    }
    let has_aggregation = !select.group_by.is_empty()
        || items
            .iter()
            .any(|c| matches!(c, Item::Agg(..) | Item::Window(_)));
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

/// Compile a flat/`GROUP BY`/`ORDER BY`/`LIMIT` query into a [`Program`]
/// ending in [`Opcode::Combine`], optionally followed by `Sort`/`Limit`
/// (db-core#48). Compiled once, reused across every segment.
pub fn compile(select: &Select) -> Program {
    let mut ctx = Ctx {
        next_reg: 0,
        column_regs: HashMap::new(),
        program: Vec::new(),
    };

    let items = classify_items(select).unwrap_or_default();
    let group_by: Vec<String> = select
        .group_by
        .iter()
        .filter_map(expr_column_name)
        .collect();

    // Load every column the group-by keys and select-list aggregates need
    // *before* compiling WHERE/Filter: Filter only shrinks registers that
    // are already live, so anything loaded afterwards would keep the
    // batch's full (pre-filter) length and desync from filtered registers.
    let mut group_by_regs = Vec::new();
    for name in &group_by {
        group_by_regs.push(ctx.load_column(name));
    }
    let mut agg_srcs = Vec::new();
    for item in &items {
        match item {
            Item::Agg(_, Some(name)) => {
                agg_srcs.push(ctx.load_column(name));
            }
            // Plain projected columns are emitted (not aggregated), but they
            // must be loaded here for the same reason as the keys above: a
            // column first loaded below the Filter keeps its full pre-filter
            // length while the filtered registers shrink, and Emit then
            // indexes past the end of the short ones. `load_column` memoizes,
            // so the projection code further down reuses these registers
            // instead of emitting a second LoadColumn.
            Item::Column(name) if group_by.is_empty() => {
                ctx.load_column(name);
                agg_srcs.push(0);
            }
            _ => agg_srcs.push(0),
        }
    }

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

    let mut aggs: Vec<(AggFunc, Option<usize>)> = Vec::new();
    let mut agg_dst = Vec::new();
    let mut emit_regs = group_by_regs.clone();

    for (item, &agg_src) in items.iter().zip(&agg_srcs) {
        if let Item::Agg(func, arg) = item {
            let src = arg.as_ref().map(|_| agg_src);
            // Resolved up front so the dispatch below has no "can't happen"
            // arm: `None` *is* the `Avg` case (the only aggregate that
            // needs two partials), not a wildcard hiding one.
            let simple_part = match func {
                AggFunc::Sum => Some(AggPart::Sum),
                AggFunc::Count => Some(AggPart::Count),
                AggFunc::Min => Some(AggPart::Min),
                AggFunc::Max => Some(AggPart::Max),
                AggFunc::Avg => None,
            };
            match simple_part {
                Some(part) => {
                    let dst = ctx.alloc();
                    aggs.push((*func, src));
                    agg_dst.push(dst);
                    agg_parts.push(part);
                    emit_regs.push(dst);
                }
                None => {
                    let sum_dst = ctx.alloc();
                    let count_dst = ctx.alloc();
                    aggs.push((AggFunc::Sum, src));
                    agg_dst.push(sum_dst);
                    aggs.push((AggFunc::Count, src));
                    agg_dst.push(count_dst);
                    let sum_pos = emit_regs.len();
                    agg_parts.push(AggPart::Avg(sum_pos, sum_pos.saturating_add(1)));
                    emit_regs.push(sum_dst);
                    emit_regs.push(count_dst);
                }
            }
        } else if let Item::Column(name) = item {
            // A plain column in the SELECT list: if there's no GROUP BY,
            // it isn't loaded/emitted anywhere else yet, so load and emit
            // it directly here. With a GROUP BY, it's expected to already
            // be one of the group-by columns (already in `emit_regs` via
            // `group_by_regs` above) -- SQL requires non-aggregated SELECT
            // columns to be group-by keys, so this doesn't double-emit.
            if group_by.is_empty() {
                let reg = ctx.load_column(name);
                emit_regs.push(reg);
            }
        }
    }

    if !aggs.is_empty() || !group_by_regs.is_empty() {
        let comment = if group_by.is_empty() {
            "aggregate".to_string()
        } else {
            format!("GROUP BY {}", group_by.join(", "))
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
        format!("SELECT {}", output_column_names(select).join(", ")),
    );

    let order_by_named = select_order_by(select);
    let order_by = order_by_named.clone().and_then(|(column, descending)| {
        select_output_index(select, &column).map(|pos| (pos, descending))
    });
    let limit = select_limit(select);
    let has_agg = classify_items(select)
        .map(|items| items.iter().any(|c| matches!(c, Item::Agg(..))))
        .unwrap_or(false);
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
            num_group_keys: group_by.len(),
            distinct: matches!(select.distinct, Some(Distinctness::Distinct)),
        },
        combine_comment,
    );
    if let Some((col, descending)) = order_by {
        let column = order_by_named.map_or_else(String::new, |(c, _)| c);
        ctx.push_commented(
            Opcode::Sort { col, descending },
            format!("ORDER BY {column}{}", if descending { " DESC" } else { "" }),
        );
    }
    if let Some(n) = limit {
        ctx.push_commented(Opcode::Limit { n }, format!("LIMIT {n}"));
    }

    Program::new(ctx.program)
}

/// Split a (possibly qualified) column name into `(table_prefix, column)`.
pub fn split_qualified(name: &str) -> (Option<&str>, &str) {
    match name.split_once('.') {
        Some((table, column)) => (Some(table), column),
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
pub fn compile_join(select: &Select) -> Result<JoinProgram> {
    let Some(from) = &select.from else {
        return Err(PlanError::UnknownColumn("SELECT without FROM".into()));
    };
    let joins = extract_joins(from)?;
    #[allow(
        clippy::expect_used,
        reason = "dispatch only routes here for queries with a JOIN clause"
    )]
    let join = joins
        .first()
        .expect("compile_join requires at least one join");
    if !matches!(join.op, JoinOp::Inner | JoinOp::Left) {
        return Err(PlanError::UnsupportedJoinKind(join.op));
    }
    let body = compile(select);

    let mut needed: Vec<String> = body.columns_to_load();
    for extra in [&join.left_col, &join.right_col] {
        if !needed.contains(extra) {
            needed.push(extra.clone());
        }
    }

    let from_name = table_name(from);
    let mut left_columns = Vec::new();
    let mut right_columns = Vec::new();
    for name in &needed {
        let (prefix, _) = split_qualified(name);
        match prefix {
            None => left_columns.push(name.clone()),
            Some(p) if p == from_name => left_columns.push(name.clone()),
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
        .map(|i| left_columns.len().saturating_add(i))
        .collect();
    // Already rejected at the top of `compile_join`; returning the same
    // error here keeps this match total without an `unreachable!` the
    // qualified subset (`make check-mvl-limit`) forbids.
    let join_kind = match join.op {
        JoinOp::Inner => crate::vm::batch::JoinKind::Inner,
        JoinOp::Left => crate::vm::batch::JoinKind::Left,
        other => return Err(PlanError::UnsupportedJoinKind(other)),
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
        body: compile(&stripped),
    })
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
pub fn compile_window(select: &Select) -> Program {
    let items = classify_items(select).unwrap_or_default();

    let mut needed: Vec<String> = Vec::new();
    let push_needed = |name: &str, needed: &mut Vec<String>| {
        if !needed.iter().any(|n| n == name) {
            needed.push(name.to_string());
        }
    };
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
            Item::Agg(..) | Item::Star => {}
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
    let mut emit_regs = Vec::with_capacity(items.len());
    for item in &items {
        match item {
            Item::Column(name) => emit_regs.push(column_reg(name)),
            Item::Window(spec) => {
                let dst = next_reg;
                next_reg = next_reg.saturating_add(1);
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
            Item::Agg(..) | Item::Star => {
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

    let order_by_named = select_order_by(select);
    let order_by = order_by_named.clone().and_then(|(column, descending)| {
        select_output_index(select, &column).map(|pos| (pos, descending))
    });
    let limit = select_limit(select);
    program.push(Instruction::with_comment(
        Opcode::Combine {
            agg_parts: Vec::new().into(),
            num_group_keys: 0,
            distinct: matches!(select.distinct, Some(Distinctness::Distinct)),
        },
        "concatenate segments".to_string(),
    ));
    if let Some((col, descending)) = order_by {
        let column = order_by_named.map_or_else(String::new, |(c, _)| c);
        program.push(Instruction::with_comment(
            Opcode::Sort { col, descending },
            format!("ORDER BY {column}{}", if descending { " DESC" } else { "" }),
        ));
    }
    if let Some(n) = limit {
        program.push(Instruction::with_comment(
            Opcode::Limit { n },
            format!("LIMIT {n}"),
        ));
    }

    Program::new(program)
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

/// Resolves an `ORDER BY` reference to its position in the `SELECT`
/// list, matching against each item's rendered output label -- not just
/// a bare column -- so `ORDER BY COUNT(x)` resolves to a `SELECT
/// COUNT(x)` item the same way `ORDER BY x` resolves to `SELECT x`
/// (#131: `parser::column`'s `ORDER BY` validation renders an aggregate
/// reference through the identical [`select_item_label`] format for
/// exactly this reason).
fn select_output_index(select: &Select, column: &str) -> Option<usize> {
    select
        .columns
        .iter()
        .position(|item| select_item_label(item) == column)
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableStats {
    /// Number of row groups (segments) in the table.
    pub row_groups: usize,
    /// Total row count of the table.
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
pub fn explain(select: &Select, stats: impl Fn(&str) -> TableStats) -> Vec<PlanNode> {
    let mut b = PlanBuilder::new("QUERY PLAN");

    let items = classify_items(select).unwrap_or_default();
    let has_window = items.iter().any(|c| matches!(c, Item::Window(_)));
    let is_semi_join = matches!(
        &select.where_clause,
        Some(AstExpr {
            kind: ExprKind::InSubquery { .. },
            ..
        })
    );
    let from = select.from.as_ref();
    let from_name = from.map(table_name).unwrap_or_default();
    let joins = from
        .map(|f| extract_joins(f).unwrap_or_default())
        .unwrap_or_default();
    let join = joins.first();

    // Semi-joins compile with `where_clause` stripped, mirroring
    // `compile_semi_join` (the `IN` subquery isn't a VM predicate).
    let program = if has_window {
        None
    } else if is_semi_join {
        let mut stripped = select.clone();
        stripped.where_clause = None;
        Some(compile(&stripped))
    } else {
        Some(compile(select))
    };
    let columns_to_load = program.as_ref().map(Program::columns_to_load);

    let mut main_cols: Vec<String> = match (&columns_to_load, join) {
        (Some(cols), Some(_)) => cols
            .iter()
            .filter(|n| split_qualified(n).0.is_none_or(|t| t == from_name))
            .cloned()
            .collect(),
        (Some(cols), None) => cols.clone(),
        (None, _) => referenced_columns(select),
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
            let sub_from = subquery.from.as_ref().map(table_name).unwrap_or_default();
            let sub_scan = b.push(0, scan_detail(sub_from, stats(sub_from)));
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

    if let Some((column, descending)) = select_order_by(select) {
        b.push(
            0,
            format!(
                "ORDER BY: {column}{}",
                if descending { " DESC" } else { "" }
            ),
        );
    }
    if let Some(limit) = select_limit(select) {
        b.push(0, format!("LIMIT: {limit}"));
    }

    let emit_labels: Vec<String> = select.columns.iter().map(select_item_label).collect();
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
        Opcode::Combine {
            agg_parts,
            num_group_keys,
            distinct,
        } => format!("agg_parts={agg_parts:?} num_group_keys={num_group_keys} distinct={distinct}"),
        Opcode::Sort { col, descending } => format!("col={col} descending={descending}"),
        Opcode::Limit { n } => format!("n={n}"),
    }
}

/// Build a bare `EXPLAIN`'s opcode listing for `select` (#55): the compiled
/// [`Program`]'s instructions, one section per phase the executor actually
/// runs -- mirrors [`explain`]'s shape dispatch (semi-join, join, windowed,
/// or plain single-table) but over the real compiled opcodes instead of a
/// hand-built plan tree.
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
            .unwrap_or_default();
        Ok(vec![OpcodeSection {
            label: format!(
                "SEMI JOIN body ({} IN (SELECT ... FROM {}))",
                semi.key_column, sub_from
            ),
            rows: render_program(&semi.body),
        }])
    } else if select.from.as_ref().is_some_and(|f| !f.joins.is_empty()) {
        let join = compile_join(select)?;
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
            rows: render_program(&compile_window(select)),
        }])
    } else {
        Ok(vec![OpcodeSection {
            label: "body".to_string(),
            rows: render_program(&compile(select)),
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
fn select_item_label(item: &ResultColumn) -> String {
    match classify_item(item) {
        Ok(Item::Column(name)) => name,
        Ok(Item::Star) => "*".to_string(),
        Ok(Item::Agg(func, arg)) => match arg {
            Some(col) => format!("{}({col})", agg_func_name(func)),
            None => format!("{}(*)", agg_func_name(func)),
        },
        Ok(Item::Window(spec)) => format!("{}()", window_func_name(spec.func)),
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
            let from = subquery.from.as_ref().map(table_name).unwrap_or_default();
            format!("{} IN (SELECT ... FROM {from})", expr_to_string(expr))
        }
        ExprKind::Exists { subquery, negated } => {
            let from = subquery.from.as_ref().map(table_name).unwrap_or_default();
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
        _ => {}
    }
}

/// Every column name `select` references, in first-seen order (used for the
/// `EXPLAIN` `LOAD COLUMNS` detail on paths that don't go through
/// [`compile`], namely windowed queries).
fn referenced_columns(select: &Select) -> Vec<String> {
    let mut out = Vec::new();
    for name in select.group_by.iter().filter_map(expr_column_name) {
        push_unique(&mut out, name);
    }
    let items = classify_items(select).unwrap_or_default();
    for item in &items {
        match item {
            Item::Column(name) => push_unique(&mut out, name.clone()),
            Item::Star => {}
            Item::Agg(_, Some(name)) => push_unique(&mut out, name.clone()),
            Item::Agg(_, None) => {}
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
            }
        }
    }
    if let Some(where_clause) = &select.where_clause {
        collect_expr_columns(where_clause, &mut out);
    }
    if let Some(from) = &select.from {
        for join in extract_joins(from).unwrap_or_default() {
            push_unique(&mut out, join.left_col.clone());
            push_unique(&mut out, join.right_col.clone());
        }
    }
    if let Some((column, _)) = select_order_by(select) {
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

    #[test]
    fn compile_distinct_sets_finalize_flag_without_group_reduce() {
        let query = sql::parse("SELECT DISTINCT a, b FROM t").unwrap();
        let program = compile(&query);
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
        let program = compile(&query);
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
        let program = compile(&query);
        let tail: Vec<&Instruction> = program.instructions.iter().rev().take(3).rev().collect();
        assert_eq!(
            tail[0].opcode,
            Opcode::Combine {
                agg_parts: Vec::new().into(),
                num_group_keys: 0,
                distinct: false,
            }
        );
        assert_eq!(tail[0].comment.as_deref(), Some("concatenate segments"));
        assert_eq!(
            tail[1].opcode,
            Opcode::Sort {
                col: 1,
                descending: true,
            }
        );
        assert_eq!(tail[1].comment.as_deref(), Some("ORDER BY val DESC"));
        assert_eq!(tail[2].opcode, Opcode::Limit { n: 5 });
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
        let program = compile(&query);
        let tail: Vec<&Instruction> = program.instructions.iter().rev().take(2).rev().collect();
        assert_eq!(
            tail[0].opcode,
            Opcode::Combine {
                agg_parts: vec![AggPart::GroupKey, AggPart::Count, AggPart::Sum].into(),
                num_group_keys: 1,
                distinct: false,
            }
        );
        assert_eq!(
            tail[1].opcode,
            Opcode::Sort {
                col: 1,
                descending: true,
            }
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__batch_829__v1_agg_without_group_by_emits_group_reduce() {
        let query = sql::parse("SELECT SUM(amount) FROM t").unwrap();
        let program = compile(&query);
        let (body, ..) = program.split_finalize();
        assert!(body
            .iter()
            .any(|op| matches!(op, Opcode::GroupReduce { .. })));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__batch_829__v2_group_by_without_agg_emits_group_reduce() {
        let query = sql::parse("SELECT region FROM t GROUP BY region").unwrap();
        let program = compile(&query);
        let (body, ..) = program.split_finalize();
        assert!(body
            .iter()
            .any(|op| matches!(op, Opcode::GroupReduce { .. })));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__batch_829__v3_no_agg_no_group_by_omits_group_reduce() {
        let query = sql::parse("SELECT id FROM t").unwrap();
        let program = compile(&query);
        let (body, ..) = program.split_finalize();
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
            Some(Opcode::Combine { .. })
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
        assert_eq!(plan.subquery.from.as_ref().map(table_name), Some("regions"));
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
        let program = compile_window(&query);
        assert_eq!(program.columns_to_load(), vec!["id", "region_key"]);
        let (body, combine, sort, limit) = program.split_finalize();
        assert!(matches!(combine, Some(Opcode::Combine { .. })));
        assert!(matches!(
            sort,
            Some(Opcode::Sort {
                col: 0,
                descending: false,
            })
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
        }
    }

    #[test]
    fn explain_plain_filter_group_by_aggregate() {
        let query = sql::parse("SELECT region, SUM(amount), COUNT(*) FROM production WHERE id > 1000 GROUP BY region ORDER BY region").unwrap();
        let nodes = explain(&query, stats);

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
        let nodes = explain(&query, stats);
        assert!(details(&nodes).contains(&"LOAD COLUMNS: orders.id, orders.region_key"));
        assert!(details(&nodes).contains(&"LOAD COLUMNS: regions.budget, regions.rkey"));
        assert!(details(&nodes).contains(&"HASH JOIN: orders.region_key = regions.rkey"));

        let query = sql::parse(
            "SELECT id FROM orders WHERE region_key IN (SELECT rkey FROM regions) ORDER BY id",
        )
        .unwrap();
        let nodes = explain(&query, stats);
        assert!(details(&nodes).contains(&"SEMI JOIN: region_key IN (SELECT rkey FROM regions)"));
        assert!(details(&nodes).contains(&"LOAD COLUMNS: id, region_key"));
        assert!(details(&nodes).contains(&"LOAD COLUMNS: rkey"));
        assert!(!details(&nodes).iter().any(|d| d.starts_with("FILTER")));

        let query = sql::parse(
            "SELECT id, region_key, ROW_NUMBER() OVER (PARTITION BY region_key ORDER BY id) \
             FROM orders ORDER BY id",
        )
        .unwrap();
        let nodes = explain(&query, stats);
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
        let nodes = explain(&query, stats);
        assert!(details(&nodes).contains(&"DISTINCT"));
        assert_eq!(nodes.last().unwrap().detail, "EMIT: region");
    }

    #[test]
    fn explain_shows_distinct_after_group_by_and_aggregate() {
        let query =
            sql::parse("SELECT DISTINCT region, SUM(amount) FROM production GROUP BY region")
                .unwrap();
        let nodes = explain(&query, stats);
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
        let nodes = explain(&query, stats);
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
        assert_eq!(finalize_row.opcode, "Combine");
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
        let query = sql::parse(
            "SELECT id, region_key, ROW_NUMBER() OVER (PARTITION BY region_key ORDER BY id) \
             FROM orders",
        )
        .unwrap();
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
        let program = compile(&expanded);
        assert_eq!(program.columns_to_load(), vec!["id", "name"]);
    }
}
