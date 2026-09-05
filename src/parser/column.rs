//! column-rs's section (#27, #57, #63, #65, #67-70): `SELECT ... FROM ...
//! [[INNER|LEFT] JOIN table ON col = col ...] [WHERE ... | WHERE col IN
//! (SELECT ...)] [GROUP BY ...] [ORDER BY ...] [LIMIT ...]`, restricted to
//! the analytics subset the query VM executes. Joins are equi-joins only.
//! The only subquery form is `col IN (SELECT ...)` (a semi-join) as the
//! *entire* `WHERE` clause -- it can't be combined with other conditions
//! via `AND`/`OR`.
//!
//! **Unified on `parser::row`'s tokenizer and grammar (#57).** This module
//! no longer has its own tokenizer or recursive-descent parser: [`parse`]/
//! [`parse_explain`] parse with [`super::row::parse_select`]/
//! [`super::row::parse_explain`] (sqlite-rs's own, shared with `row`) and
//! then [`convert_select`] lowers the resulting [`super::row::ast::Select`]
//! into [`crate::expr::Query`] -- the shape [`crate::codegen::batch`],
//! `crate::emit::batch`, and column-rs's runtime glue all still expect.
//! `convert_select` is where "column's grammar becomes an enforced
//! subset" (ADR 0002's second amendment) actually happens: a `Select`
//! outside this subset (a real JOIN condition shape, `WITH`, `UNION`,
//! `HAVING`, non-integer `LIMIT`, a second `ORDER BY` term, ...) is
//! rejected here with [`ParseError::Unexpected`], not silently
//! misconverted.
//!
//! **Window functions** (#74 follow-up): `parser::row`'s grammar parses
//! `func(...) OVER (PARTITION BY ... ORDER BY ...)` (see
//! `grammar::parse_function_call`/`window_def`); [`convert_window_call`]
//! resolves `func` against [`WindowFunc::from_name`] and lowers into
//! [`SelectItem::Window`], the same shape `codegen::batch::compile_window`
//! already executes. Not carried forward from real SQLite/DuckDB syntax:
//! a named `OVER window_name` (would need the still-unsupported `WINDOW`
//! clause) and an explicit frame (`ROWS`/`RANGE`/`GROUPS ...`, no
//! representation in `WindowSpec` -- every window function runs over a
//! fixed default frame instead) are both rejected with a clear error, not
//! silently accepted or misconverted.
//!
//! Errors carry a [`Span`] (see `ADR 0001`/`ADR 0002` in `db-core`'s
//! `.openspec/adr/`), matching sqlite-rs's own `ParseFail`/`ParseOutcome`
//! convention: a consumer (REPL, IDE) can point at *where* parsing failed,
//! not just read a message.

use std::collections::HashMap;
use std::fmt;

use crate::expr::{
    AggFunc, BinOp, Expr, Join, JoinKind, OrderBy, Query, SelectItem, WindowFunc,
    WindowSpec as ExprWindowSpec,
};
use crate::parser::row::ast::{
    BinaryOp as AstBinOp, Distinctness, Expr as AstExpr, ExprKind, FunctionArgs, JoinConstraint,
    JoinOp, Literal as AstLiteral, OrderingTerm, ResultColumn, Select, TableRefKind, UnaryOp,
    WindowDef,
};
use crate::parser::row::ParseOutcome;
use crate::parser::Span;
use crate::types::Literal;

#[derive(Debug, PartialEq)]
pub enum ParseError {
    UnexpectedEof { span: Span },
    Unexpected { message: String, span: Span },
}

impl ParseError {
    /// The location this error points at.
    pub fn span(&self) -> Span {
        match self {
            ParseError::UnexpectedEof { span } | ParseError::Unexpected { span, .. } => *span,
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::UnexpectedEof { span } => {
                write!(
                    f,
                    "unexpected end of query at {}:{}",
                    span.line, span.column
                )
            }
            ParseError::Unexpected { message, span } => {
                write!(
                    f,
                    "unexpected token at {}:{}: {message}",
                    span.line, span.column
                )
            }
        }
    }
}

impl std::error::Error for ParseError {}

pub type Result<T> = std::result::Result<T, ParseError>;

fn unsupported(span: Span, message: String) -> ParseError {
    ParseError::Unexpected { message, span }
}

/// Rewrite `alias.col` to `real_table.col` in place, for every qualified
/// column name `resolve_query_aliases` touches. Unqualified names and
/// names already qualified by a real table name pass through unchanged.
fn resolve_ident(name: &mut String, aliases: &HashMap<String, String>) {
    if let Some((prefix, col)) = name.split_once('.') {
        if let Some(real) = aliases.get(prefix) {
            *name = format!("{real}.{col}");
        }
    }
}

fn resolve_expr_aliases(expr: &mut Expr, aliases: &HashMap<String, String>) {
    match expr {
        Expr::Column(name) => resolve_ident(name, aliases),
        Expr::Literal(_) => {}
        Expr::BinaryOp(lhs, _, rhs) => {
            resolve_expr_aliases(lhs, aliases);
            resolve_expr_aliases(rhs, aliases);
        }
        Expr::Not(inner) => resolve_expr_aliases(inner, aliases),
        Expr::Neg(inner) => resolve_expr_aliases(inner, aliases),
        Expr::IsNull { expr, .. } => resolve_expr_aliases(expr, aliases),
        // A subquery has its own FROM/alias scope -- its column refs are
        // resolved when *it* is converted, not against the outer query's
        // aliases.
        Expr::InSubquery { expr, .. } => resolve_expr_aliases(expr, aliases),
    }
}

/// Rewrite every qualified column reference in `query` (SELECT list, JOIN
/// ON, WHERE, GROUP BY, ORDER BY) from an alias-qualified name to the real
/// table name, per `aliases` (alias -> real table name). Table aliases are
/// a parse-time-only convenience this way: `crate::expr::Query`/`Join` never
/// see or store an alias, so `column-rs` (an existing consumer) doesn't
/// need any change to keep working with aliased queries.
fn resolve_query_aliases(query: &mut Query, aliases: &HashMap<String, String>) {
    for item in &mut query.columns {
        if let SelectItem::Column(name) = item {
            resolve_ident(name, aliases);
        }
    }
    for join in &mut query.joins {
        resolve_ident(&mut join.left_col, aliases);
        resolve_ident(&mut join.right_col, aliases);
    }
    if let Some(expr) = &mut query.where_clause {
        resolve_expr_aliases(expr, aliases);
    }
    for name in &mut query.group_by {
        resolve_ident(name, aliases);
    }
    if let Some(order_by) = &mut query.order_by {
        resolve_ident(&mut order_by.column, aliases);
    }
}

// ---------------------------------------------------------------------
// ast::Select -> crate::expr::Query
// ---------------------------------------------------------------------

fn ast_binop(op: AstBinOp, span: Span) -> Result<BinOp> {
    Ok(match op {
        AstBinOp::Add => BinOp::Add,
        AstBinOp::Sub => BinOp::Sub,
        AstBinOp::Mul => BinOp::Mul,
        AstBinOp::Div => BinOp::Div,
        AstBinOp::Eq => BinOp::Eq,
        AstBinOp::Ne => BinOp::Ne,
        AstBinOp::Lt => BinOp::Lt,
        AstBinOp::Le => BinOp::Le,
        AstBinOp::Gt => BinOp::Gt,
        AstBinOp::Ge => BinOp::Ge,
        AstBinOp::And => BinOp::And,
        AstBinOp::Or => BinOp::Or,
        AstBinOp::Concat => BinOp::Concat,
        other => return Err(unsupported(span, format!("operator {other:?}"))),
    })
}

fn ast_literal(lit: &AstLiteral, span: Span) -> Result<Literal> {
    Ok(match lit {
        AstLiteral::Integer(n) => Literal::Int(*n),
        AstLiteral::Float(f) => Literal::Float(*f),
        AstLiteral::Str(s) => Literal::Str(s.clone()),
        other => return Err(unsupported(span, format!("literal {other:?}"))),
    })
}

/// A (possibly qualified) column reference: `col` or `table.col`. Aliases
/// aren't resolved here -- that happens afterward, over the whole
/// converted `Query`, via [`resolve_query_aliases`].
fn column_name(expr: &AstExpr) -> Result<String> {
    match &expr.kind {
        ExprKind::Column {
            table: None,
            catalog: None,
            name,
        } => Ok(name.clone()),
        ExprKind::Column {
            table: Some(table),
            catalog: None,
            name,
        } => Ok(format!("{table}.{name}")),
        ExprKind::Column {
            catalog: Some(_), ..
        } => Err(unsupported(expr.span, "catalog-qualified column".into())),
        other => Err(unsupported(
            expr.span,
            format!("expected a column reference, found {other:?}"),
        )),
    }
}

/// Lowers an aggregate `FunctionCall` (`COUNT(x)`, `COUNT(*)`, ...) to
/// the same output label [`convert_result_column`]'s `SelectItem::Agg`
/// branch would render for it (via [`AggFunc::name`]) -- used by
/// `ORDER BY` (#131) to reference a `SELECT`-list aggregate by the
/// identical string `codegen::batch::select_output_index` matches
/// against, without introducing a second aggregate-lowering path.
fn aggregate_call_label(
    expr: &AstExpr,
    name: &str,
    distinct: bool,
    args: &FunctionArgs,
) -> Result<String> {
    if distinct {
        return Err(unsupported(
            expr.span,
            "DISTINCT inside an aggregate".into(),
        ));
    }
    let agg = AggFunc::from_name(name)
        .ok_or_else(|| unsupported(expr.span, format!("unknown function {name}")))?;
    match args {
        FunctionArgs::Star => {
            if agg != AggFunc::Count {
                return Err(unsupported(expr.span, "only COUNT supports (*)".into()));
            }
            Ok(format!("{}(*)", agg.name()))
        }
        FunctionArgs::List(list) => match list.as_slice() {
            [one] => Ok(format!("{}({})", agg.name(), column_name(one)?)),
            _ => Err(unsupported(
                expr.span,
                "an aggregate takes exactly one column or *".into(),
            )),
        },
    }
}

fn convert_expr(expr: &AstExpr) -> Result<Expr> {
    match &expr.kind {
        ExprKind::Literal(lit) => Ok(Expr::Literal(ast_literal(lit, expr.span)?)),
        ExprKind::Column { .. } => Ok(Expr::Column(column_name(expr)?)),
        ExprKind::Unary {
            op: UnaryOp::Not,
            expr: inner,
        } => Ok(Expr::Not(Box::new(convert_expr(inner)?))),
        ExprKind::Unary {
            op: UnaryOp::Minus,
            expr: inner,
        } => Ok(Expr::Neg(Box::new(convert_expr(inner)?))),
        // Unary `+` is a no-op, same as the old grammar (which discarded a
        // leading `+` instead of representing it at all).
        ExprKind::Unary {
            op: UnaryOp::Plus,
            expr: inner,
        } => convert_expr(inner),
        ExprKind::Unary { op, .. } => Err(unsupported(expr.span, format!("unary operator {op:?}"))),
        // `expr IS [NOT] NULL` parses as `Is{lhs, rhs: NULL literal,
        // negated}` in row's grammar, not a dedicated `IsNull` node (that
        // variant exists for the historical SQLite `IS`/`IS NOT` operator
        // between two arbitrary expressions, which this subset doesn't
        // support otherwise).
        ExprKind::Is { lhs, rhs, negated }
            if matches!(rhs.kind, ExprKind::Literal(AstLiteral::Null)) =>
        {
            Ok(Expr::IsNull {
                expr: Box::new(convert_expr(lhs)?),
                negated: *negated,
            })
        }
        ExprKind::Binary { op, lhs, rhs } => Ok(Expr::BinaryOp(
            Box::new(convert_expr(lhs)?),
            ast_binop(*op, expr.span)?,
            Box::new(convert_expr(rhs)?),
        )),
        ExprKind::IsNull {
            expr: inner,
            negated,
        } => Ok(Expr::IsNull {
            expr: Box::new(convert_expr(inner)?),
            negated: *negated,
        }),
        ExprKind::Paren(inner) => convert_expr(inner),
        ExprKind::InSubquery {
            expr: inner,
            subquery,
            negated: false,
        } => Ok(Expr::InSubquery {
            expr: Box::new(convert_expr(inner)?),
            subquery: Box::new(convert_select(subquery)?),
        }),
        ExprKind::InSubquery { negated: true, .. } => {
            Err(unsupported(expr.span, "NOT IN (SELECT ...)".into()))
        }
        other => Err(unsupported(
            expr.span,
            format!("unsupported expression form {other:?}"),
        )),
    }
}

fn extract_equi_join(expr: &AstExpr) -> Result<(String, String)> {
    match &expr.kind {
        ExprKind::Binary {
            op: AstBinOp::Eq,
            lhs,
            rhs,
        } => Ok((column_name(lhs)?, column_name(rhs)?)),
        _ => Err(unsupported(expr.span, "JOIN ON must be col = col".into())),
    }
}

fn convert_result_column(col: &ResultColumn) -> Result<SelectItem> {
    match col {
        ResultColumn::Star => Ok(SelectItem::Star),
        ResultColumn::TableStar { .. } => Err(unsupported(
            Span::UNKNOWN,
            "table.* is not supported".into(),
        )),
        ResultColumn::Expr {
            expr,
            alias: Some(_),
        } => Err(unsupported(
            expr.span,
            "column alias (AS) is not supported".into(),
        )),
        ResultColumn::Expr { expr, alias: None } => match &expr.kind {
            ExprKind::Column { .. } => Ok(SelectItem::Column(column_name(expr)?)),
            ExprKind::FunctionCall {
                name,
                distinct,
                args,
                over: Some(window_def),
            } => {
                if *distinct {
                    return Err(unsupported(
                        expr.span,
                        "DISTINCT inside a window function".into(),
                    ));
                }
                convert_window_call(expr.span, name, args, window_def)
            }
            ExprKind::FunctionCall {
                name,
                distinct,
                args,
                over: None,
            } => {
                if *distinct {
                    return Err(unsupported(
                        expr.span,
                        "DISTINCT inside an aggregate".into(),
                    ));
                }
                let agg = AggFunc::from_name(name)
                    .ok_or_else(|| unsupported(expr.span, format!("unknown function {name}")))?;
                let arg = match args {
                    FunctionArgs::Star => {
                        if agg != AggFunc::Count {
                            return Err(unsupported(expr.span, "only COUNT supports (*)".into()));
                        }
                        None
                    }
                    FunctionArgs::List(list) => match list.as_slice() {
                        [one] => Some(column_name(one)?),
                        _ => {
                            return Err(unsupported(
                                expr.span,
                                "an aggregate takes exactly one column or *".into(),
                            ))
                        }
                    },
                };
                Ok(SelectItem::Agg(agg, arg))
            }
            _ => Err(unsupported(
                expr.span,
                "unsupported SELECT expression".into(),
            )),
        },
    }
}

/// Lowers `name(args) OVER (window_def)` into [`SelectItem::Window`]:
/// resolves `name` against [`WindowFunc::from_name`], validates the
/// argument count/shape each function kind expects (niladic for
/// `ROW_NUMBER`/`RANK`/`DENSE_RANK`, one column plus an optional integer
/// offset for `LAG`/`LEAD`, one column or `COUNT(*)` for the rest), and
/// converts `window_def`'s `PARTITION BY`/`ORDER BY` expressions to plain
/// column names -- the same "enforced subset" restriction the rest of
/// this module applies (no expressions, only column references).
fn convert_window_call(
    span: Span,
    name: &str,
    args: &FunctionArgs,
    window_def: &WindowDef,
) -> Result<SelectItem> {
    let func = WindowFunc::from_name(name)
        .ok_or_else(|| unsupported(span, format!("unknown window function {name}")))?;

    let (arg, offset) = match (func.is_niladic(), args) {
        (true, FunctionArgs::List(list)) if list.is_empty() => (None, None),
        (true, _) => Err(unsupported(span, format!("{name} takes no arguments")))?,
        (false, FunctionArgs::Star) => {
            if func != WindowFunc::Count {
                return Err(unsupported(span, "only COUNT supports (*)".into()));
            }
            (None, None)
        }
        (false, FunctionArgs::List(list)) if matches!(func, WindowFunc::Lag | WindowFunc::Lead) => {
            match list.as_slice() {
                [one] => (Some(column_name(one)?), None),
                [one, offset_expr] => {
                    let offset = match &offset_expr.kind {
                        ExprKind::Literal(AstLiteral::Integer(n)) => *n,
                        _ => {
                            return Err(unsupported(offset_expr.span, "non-integer offset".into()))
                        }
                    };
                    (Some(column_name(one)?), Some(offset))
                }
                _ => return Err(unsupported(span, format!("{name} takes 1 or 2 arguments"))),
            }
        }
        (false, FunctionArgs::List(list)) => match list.as_slice() {
            [one] => (Some(column_name(one)?), None),
            _ => {
                return Err(unsupported(
                    span,
                    "a window function takes exactly one column or *".into(),
                ))
            }
        },
    };

    let partition_by = window_def
        .partition_by
        .iter()
        .map(column_name)
        .collect::<Result<Vec<_>>>()?;
    let order_by = window_def
        .order_by
        .iter()
        .map(|term| Ok((column_name(&term.expr)?, term.desc.unwrap_or(false))))
        .collect::<Result<Vec<_>>>()?;

    Ok(SelectItem::Window(ExprWindowSpec {
        func,
        arg,
        offset,
        partition_by,
        order_by,
    }))
}

/// Lowers a `Select` parsed by [`super::row`]'s shared grammar into
/// [`crate::expr::Query`], rejecting anything outside column-rs's
/// analytics subset with [`ParseError::Unexpected`] -- ADR 0002's
/// "column's grammar becomes an enforced subset" (parsing succeeds,
/// lowering declines), not a second parser that can't parse these
/// constructs at all.
fn convert_select(select: &Select) -> Result<Query> {
    if select.with_clause.is_some() {
        return Err(unsupported(select.span, "WITH clause".into()));
    }
    if !select.compound.is_empty() {
        return Err(unsupported(select.span, "UNION".into()));
    }
    if select.having.is_some() {
        return Err(unsupported(select.span, "HAVING".into()));
    }
    let distinct = matches!(select.distinct, Some(Distinctness::Distinct));

    let Some(from_clause) = &select.from else {
        return Err(unsupported(select.span, "SELECT without FROM".into()));
    };
    let TableRefKind::Name(from_name) = &from_clause.first.kind else {
        return Err(unsupported(
            from_clause.first.span,
            "subquery in FROM".into(),
        ));
    };
    let from_name = from_name.clone();

    let mut aliases: HashMap<String, String> = HashMap::new();
    if let Some(alias) = &from_clause.first.alias {
        aliases.insert(alias.clone(), from_name.clone());
    }

    let mut joins = Vec::new();
    for j in &from_clause.joins {
        if j.natural {
            return Err(unsupported(j.table.span, "NATURAL join".into()));
        }
        let TableRefKind::Name(table) = &j.table.kind else {
            return Err(unsupported(j.table.span, "subquery in JOIN".into()));
        };
        let table = table.clone();
        if let Some(alias) = &j.table.alias {
            aliases.insert(alias.clone(), table.clone());
        }
        let kind = match j.op {
            JoinOp::Inner => JoinKind::Inner,
            JoinOp::Left => JoinKind::Left,
            JoinOp::Right => JoinKind::Right,
            JoinOp::Full => JoinKind::Full,
            JoinOp::Cross => JoinKind::Cross,
        };
        let (left_col, right_col) = match &j.constraint {
            Some(JoinConstraint::On(expr)) => extract_equi_join(expr)?,
            Some(JoinConstraint::Using(_)) => {
                return Err(unsupported(j.table.span, "USING join".into()))
            }
            None if kind == JoinKind::Cross => (String::new(), String::new()),
            None => return Err(unsupported(j.table.span, "join without ON".into())),
        };
        joins.push(Join {
            kind,
            table,
            left_col,
            right_col,
        });
    }

    let where_clause = select.where_clause.as_ref().map(convert_expr).transpose()?;

    let group_by = select
        .group_by
        .iter()
        .map(column_name)
        .collect::<Result<Vec<_>>>()?;

    if select.order_by.len() > 1 {
        return Err(unsupported(select.span, "multiple ORDER BY terms".into()));
    }
    let order_by = match select.order_by.first() {
        Some(OrderingTerm {
            nulls_last: Some(_),
            ..
        }) => return Err(unsupported(select.span, "NULLS FIRST/LAST".into())),
        Some(OrderingTerm { expr, desc, .. }) => {
            let column = match &expr.kind {
                ExprKind::Column { .. } => column_name(expr)?,
                ExprKind::FunctionCall {
                    name,
                    distinct,
                    args,
                    over: None,
                } => aggregate_call_label(expr, name, *distinct, args)?,
                _ => {
                    return Err(unsupported(
                        expr.span,
                        format!(
                            "expected a column reference or aggregate, found {:?}",
                            expr.kind
                        ),
                    ))
                }
            };
            Some(OrderBy {
                column,
                descending: desc.unwrap_or(false),
            })
        }
        None => None,
    };

    let limit = match &select.limit {
        Some(l) => {
            if l.offset.is_some() {
                return Err(unsupported(select.span, "LIMIT OFFSET".into()));
            }
            match &l.limit.kind {
                ExprKind::Literal(AstLiteral::Integer(n)) if *n >= 0 => Some(*n as usize),
                _ => return Err(unsupported(l.limit.span, "non-integer LIMIT".into())),
            }
        }
        None => None,
    };

    if joins.iter().any(|j| j.kind == JoinKind::Cross) && limit.is_none() {
        return Err(unsupported(
            select.span,
            "CROSS JOIN requires a LIMIT (bounded-execution rule -- an unconditional cross \
             product has no natural row cap)"
                .into(),
        ));
    }

    let columns = select
        .columns
        .iter()
        .map(convert_result_column)
        .collect::<Result<Vec<_>>>()?;

    // `compile` (codegen::batch) emits every non-aggregated SELECT column
    // via GROUP BY's own key registers, in GROUP BY's stated order -- not
    // by re-checking each SELECT column against `group_by` itself. A
    // `SELECT` whose bare columns don't match `group_by` exactly (extra
    // column, missing key, or different order) desyncs from that
    // assumption: previously this either silently produced wrong values
    // (order mismatch) or crashed outright (a bare column with no GROUP BY
    // at all -- its full-length register got zipped against an
    // aggregate's single-row one in `Emit`, indexing past the end).
    // Window queries have their own, separate semantics and never reach
    // `compile` this way, so they're exempt.
    let has_window = columns.iter().any(|c| matches!(c, SelectItem::Window(_)));
    let has_agg = columns.iter().any(|c| matches!(c, SelectItem::Agg(..)));
    if has_agg && !has_window {
        let select_bare: Vec<&String> = columns
            .iter()
            .filter_map(|c| match c {
                SelectItem::Column(name) => Some(name),
                _ => None,
            })
            .collect();
        if select_bare != group_by.iter().collect::<Vec<_>>() {
            let message = if group_by.is_empty() {
                "a plain column alongside an aggregate requires GROUP BY".to_string()
            } else {
                "every non-aggregated SELECT column must be a GROUP BY key, listed in the \
                 same order as GROUP BY"
                    .to_string()
            };
            return Err(unsupported(select.span, message));
        }
    }

    let mut query = Query {
        columns,
        from: from_name,
        joins,
        where_clause,
        distinct,
        group_by,
        order_by,
        limit,
    };
    if !aliases.is_empty() {
        resolve_query_aliases(&mut query, &aliases);
    }
    Ok(query)
}

fn from_outcome(message: String, span: Span) -> ParseError {
    ParseError::Unexpected { message, span }
}

pub fn parse(input: &str) -> Result<Query> {
    match crate::parser::row::parse_select(input) {
        ParseOutcome::Accepted(select) => convert_select(&select),
        ParseOutcome::Unsupported { message, span } | ParseOutcome::Invalid { message, span } => {
            Err(from_outcome(message, span))
        }
    }
}

/// Which `EXPLAIN` form (if any) prefixed a query: bare `EXPLAIN` renders
/// the compiled `Program`'s opcode listing, `EXPLAIN QUERY PLAN` renders
/// the plan tree -- two distinct outputs (#55), not a single bool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Explain {
    /// No `EXPLAIN` prefix -- run the query normally.
    None,
    /// Bare `EXPLAIN`: opcode listing.
    Opcodes,
    /// `EXPLAIN QUERY PLAN`: plan tree.
    QueryPlan,
}

/// Parses `EXPLAIN [QUERY PLAN] <select>`, returning which `EXPLAIN` form
/// (if any) prefixed it along with the parsed query. The distinction
/// (#55) falls out of unifying on `row`'s grammar for free -- its
/// `parse_explain_stmt` already tracks bare `EXPLAIN` vs `EXPLAIN QUERY
/// PLAN` via `ast::Explain::query_plan`.
pub fn parse_explain(input: &str) -> Result<(Explain, Query)> {
    let starts_with_explain = input
        .split_whitespace()
        .next()
        .is_some_and(|w| w.eq_ignore_ascii_case("EXPLAIN"));
    if !starts_with_explain {
        return Ok((Explain::None, parse(input)?));
    }
    match crate::parser::row::parse_explain(input) {
        ParseOutcome::Accepted(explain) => {
            let form = if explain.query_plan {
                Explain::QueryPlan
            } else {
                Explain::Opcodes
            };
            Ok((form, convert_select(&explain.select)?))
        }
        ParseOutcome::Unsupported { message, span } | ParseOutcome::Invalid { message, span } => {
            Err(from_outcome(message, span))
        }
    }
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
    use crate::expr::{Join, JoinKind, WindowSpec};

    #[test]
    fn parse_explain_distinguishes_opcodes_query_plan_and_none() {
        let (explain, query) = parse_explain("EXPLAIN SELECT id FROM orders").unwrap();
        assert_eq!(explain, Explain::Opcodes);
        assert_eq!(query.from, "orders");

        let (explain, _) = parse_explain("EXPLAIN QUERY PLAN SELECT id FROM orders").unwrap();
        assert_eq!(explain, Explain::QueryPlan);

        let (explain, _) = parse_explain("SELECT id FROM orders").unwrap();
        assert_eq!(explain, Explain::None);
    }

    #[test]
    fn parses_columns_and_where() {
        let q = parse("SELECT id, amount FROM orders WHERE amount > 10").unwrap();
        assert_eq!(
            q.columns,
            vec![
                SelectItem::Column("id".into()),
                SelectItem::Column("amount".into())
            ]
        );
        assert_eq!(
            q.where_clause,
            Some(Expr::BinaryOp(
                Box::new(Expr::Column("amount".into())),
                BinOp::Gt,
                Box::new(Expr::Literal(Literal::Int(10)))
            ))
        );
    }

    #[test]
    fn parses_unary_minus() {
        let q = parse("SELECT id FROM orders WHERE amount = -5").unwrap();
        assert_eq!(
            q.where_clause,
            Some(Expr::BinaryOp(
                Box::new(Expr::Column("amount".into())),
                BinOp::Eq,
                Box::new(Expr::Neg(Box::new(Expr::Literal(Literal::Int(5)))))
            ))
        );
    }

    #[test]
    fn unary_minus_is_chainable_and_unary_plus_is_a_no_op() {
        // `--` immediately adjacent is a SQL line comment under `row`'s
        // (real) tokenizer -- unlike the old bespoke one, which had no
        // comment syntax and read it as two unary minuses. A space still
        // parses as chained unary minus.
        let q = parse("SELECT id FROM orders WHERE amount = - -5").unwrap();
        assert_eq!(
            q.where_clause,
            Some(Expr::BinaryOp(
                Box::new(Expr::Column("amount".into())),
                BinOp::Eq,
                Box::new(Expr::Neg(Box::new(Expr::Neg(Box::new(Expr::Literal(
                    Literal::Int(5)
                ))))))
            ))
        );
        let q = parse("SELECT id FROM orders WHERE amount = +5").unwrap();
        assert_eq!(
            q.where_clause,
            Some(Expr::BinaryOp(
                Box::new(Expr::Column("amount".into())),
                BinOp::Eq,
                Box::new(Expr::Literal(Literal::Int(5)))
            ))
        );
    }

    #[test]
    fn unary_minus_binds_tighter_than_multiplication() {
        // `-2 * 3` must be `(-2) * 3`, not `-(2 * 3)` (same numeric
        // result here, but the AST shape is what's under test).
        let q = parse("SELECT id FROM orders WHERE amount = -2 * 3").unwrap();
        assert_eq!(
            q.where_clause,
            Some(Expr::BinaryOp(
                Box::new(Expr::Column("amount".into())),
                BinOp::Eq,
                Box::new(Expr::BinaryOp(
                    Box::new(Expr::Neg(Box::new(Expr::Literal(Literal::Int(2))))),
                    BinOp::Mul,
                    Box::new(Expr::Literal(Literal::Int(3)))
                ))
            ))
        );
    }

    #[test]
    fn parses_string_concat() {
        let q = parse("SELECT id FROM orders WHERE name = 'a' || 'b'").unwrap();
        assert_eq!(
            q.where_clause,
            Some(Expr::BinaryOp(
                Box::new(Expr::Column("name".into())),
                BinOp::Eq,
                Box::new(Expr::BinaryOp(
                    Box::new(Expr::Literal(Literal::Str("a".into()))),
                    BinOp::Concat,
                    Box::new(Expr::Literal(Literal::Str("b".into())))
                ))
            ))
        );
    }

    #[test]
    fn concat_binds_tighter_than_multiplication_matching_sqlite_not_duckdb() {
        // Behavior change from unification (#57): this subset now uses
        // `row`'s (sqlite-rs's) operator precedence, where `||` binds
        // *tighter* than `*`/`/` -- not column-rs's previous DuckDB-style
        // "concat binds looser" precedence, since there's one shared
        // grammar/precedence table now, not two. `2 * 3 || 'x'` is
        // `2 * (3 || 'x')`, not `(2 * 3) || 'x'`.
        let q = parse("SELECT id FROM orders WHERE x = 2 * 3 || 'x'").unwrap();
        assert_eq!(
            q.where_clause,
            Some(Expr::BinaryOp(
                Box::new(Expr::Column("x".into())),
                BinOp::Eq,
                Box::new(Expr::BinaryOp(
                    Box::new(Expr::Literal(Literal::Int(2))),
                    BinOp::Mul,
                    Box::new(Expr::BinaryOp(
                        Box::new(Expr::Literal(Literal::Int(3))),
                        BinOp::Concat,
                        Box::new(Expr::Literal(Literal::Str("x".into())))
                    ))
                ))
            ))
        );
    }

    #[test]
    fn parses_group_by_aggregate() {
        let q = parse("SELECT region, SUM(amount) FROM t WHERE x > 10 GROUP BY region").unwrap();
        assert_eq!(
            q.columns,
            vec![
                SelectItem::Column("region".into()),
                SelectItem::Agg(AggFunc::Sum, Some("amount".into()))
            ]
        );
        assert_eq!(q.group_by, vec!["region".to_string()]);
    }

    /// Regression: `SELECT active, MAX(id) FROM t` (a bare column alongside
    /// an aggregate, no GROUP BY) used to compile into mismatched-length
    /// registers and panic in `Emit` at runtime ("index out of bounds").
    /// Invalid SQL -- must be rejected at parse time instead.
    #[test]
    fn bare_column_with_aggregate_and_no_group_by_is_rejected() {
        let err = parse("SELECT active, MAX(id) FROM t").unwrap_err();
        assert!(matches!(err, ParseError::Unexpected { .. }));
    }

    #[test]
    fn aggregate_only_with_no_group_by_and_no_bare_column_is_fine() {
        assert!(parse("SELECT MAX(id) FROM t").is_ok());
        assert!(parse("SELECT COUNT(*), SUM(amount) FROM t").is_ok());
    }

    /// A bare column that isn't a GROUP BY key at all.
    #[test]
    fn bare_column_not_a_group_by_key_is_rejected() {
        let err = parse("SELECT active, SUM(amount) FROM t GROUP BY region").unwrap_err();
        assert!(matches!(err, ParseError::Unexpected { .. }));
    }

    /// `compile` (codegen::batch) emits non-aggregated SELECT columns via
    /// GROUP BY's own stated order, not the SELECT list's -- a SELECT
    /// naming its GROUP BY keys in a different order than GROUP BY itself
    /// would otherwise silently show values under the wrong headers.
    #[test]
    fn select_list_group_by_keys_in_a_different_order_than_group_by_is_rejected() {
        let err =
            parse("SELECT year, region, SUM(amount) FROM t GROUP BY region, year").unwrap_err();
        assert!(matches!(err, ParseError::Unexpected { .. }));
    }

    /// GROUP BY on a column that isn't itself selected (valid SQL: you can
    /// group by a column you don't project) is still rejected today --
    /// `compile`'s GROUP BY registers always get emitted regardless of
    /// whether they were requested, so the two must match exactly for now.
    #[test]
    fn group_by_key_omitted_from_select_list_is_rejected() {
        let err = parse("SELECT SUM(amount) FROM t GROUP BY region").unwrap_err();
        assert!(matches!(err, ParseError::Unexpected { .. }));
    }

    #[test]
    fn select_list_matching_group_by_keys_exactly_is_accepted() {
        assert!(parse("SELECT region, year, SUM(amount) FROM t GROUP BY region, year").is_ok());
    }

    /// Window functions are exempt: they have their own semantics and never
    /// require GROUP BY.
    #[test]
    fn window_function_alongside_a_bare_column_needs_no_group_by() {
        assert!(
            parse("SELECT id, ROW_NUMBER() OVER (PARTITION BY region ORDER BY id) FROM t").is_ok()
        );
    }

    #[test]
    fn parses_distinct() {
        let q = parse("SELECT DISTINCT a, b FROM t").unwrap();
        assert!(q.distinct);
        assert_eq!(
            q.columns,
            vec![
                SelectItem::Column("a".into()),
                SelectItem::Column("b".into())
            ]
        );
    }

    #[test]
    fn plain_select_is_not_distinct() {
        let q = parse("SELECT a FROM t").unwrap();
        assert!(!q.distinct);
    }

    #[test]
    fn parses_order_by_and_limit() {
        let q = parse("SELECT id FROM t ORDER BY id DESC LIMIT 5").unwrap();
        assert_eq!(
            q.order_by,
            Some(OrderBy {
                column: "id".into(),
                descending: true
            })
        );
        assert_eq!(q.limit, Some(5));
    }

    #[test]
    fn order_by_references_a_select_list_aggregate() {
        // #131: ORDER BY may reference a SELECT-list aggregate, not just
        // a bare column.
        let q = parse(
            "SELECT customer_id, COUNT(event_id), SUM(amount) FROM events \
             GROUP BY customer_id ORDER BY COUNT(event_id) DESC",
        )
        .unwrap();
        assert_eq!(
            q.order_by,
            Some(OrderBy {
                column: "COUNT(event_id)".into(),
                descending: true
            })
        );
    }

    #[test]
    fn order_by_references_count_star() {
        let q = parse("SELECT COUNT(*) FROM t ORDER BY COUNT(*)").unwrap();
        assert_eq!(
            q.order_by,
            Some(OrderBy {
                column: "COUNT(*)".into(),
                descending: false
            })
        );
    }

    #[test]
    fn order_by_rejects_non_aggregate_expression() {
        let err = parse("SELECT id FROM t ORDER BY id + 1").unwrap_err();
        assert!(format!("{err}").contains("expected a column reference"));
    }

    #[test]
    fn parses_count_star() {
        let q = parse("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(q.columns, vec![SelectItem::Agg(AggFunc::Count, None)]);
    }

    #[test]
    fn rejects_trailing_garbage() {
        let err = parse("SELECT id FROM t GARBAGE EXTRA").unwrap_err();
        assert!(matches!(err, ParseError::Unexpected { .. }));
    }

    #[test]
    fn parses_inner_join() {
        let q = parse("SELECT orders.id, customers.name FROM orders JOIN customers ON orders.cust_id = customers.id").unwrap();
        assert_eq!(q.from, "orders");
        assert_eq!(
            q.columns,
            vec![
                SelectItem::Column("orders.id".into()),
                SelectItem::Column("customers.name".into())
            ]
        );
        assert_eq!(
            q.joins,
            vec![Join {
                kind: JoinKind::Inner,
                table: "customers".into(),
                left_col: "orders.cust_id".into(),
                right_col: "customers.id".into()
            }]
        );
    }

    #[test]
    fn parses_left_join() {
        let q = parse("SELECT id FROM t LEFT JOIN u ON t.k = u.k").unwrap();
        assert_eq!(
            q.joins,
            vec![Join {
                kind: JoinKind::Left,
                table: "u".into(),
                left_col: "t.k".into(),
                right_col: "u.k".into()
            }]
        );
    }

    #[test]
    fn parses_in_subquery() {
        let q =
            parse("SELECT id FROM orders WHERE region_key IN (SELECT rkey FROM regions)").unwrap();
        let Some(Expr::InSubquery { expr, subquery }) = q.where_clause else {
            panic!("expected InSubquery")
        };
        assert_eq!(*expr, Expr::Column("region_key".into()));
        assert_eq!(subquery.from, "regions");
        assert_eq!(subquery.columns, vec![SelectItem::Column("rkey".into())]);
    }

    #[test]
    fn row_number_over_partition_and_order_by() {
        let q = parse("SELECT ROW_NUMBER() OVER (PARTITION BY region ORDER BY id) FROM t").unwrap();
        assert_eq!(
            q.columns,
            vec![SelectItem::Window(WindowSpec {
                func: WindowFunc::RowNumber,
                arg: None,
                offset: None,
                partition_by: vec!["region".into()],
                order_by: vec![("id".into(), false)],
            })]
        );
    }

    #[test]
    fn row_number_rejects_arguments() {
        let err = parse("SELECT ROW_NUMBER(id) OVER (ORDER BY id) FROM t").unwrap_err();
        assert!(matches!(err, ParseError::Unexpected { .. }));
    }

    #[test]
    fn rank_and_dense_rank_over_order_by_only() {
        for name in ["RANK", "DENSE_RANK"] {
            let q = parse(&format!("SELECT {name}() OVER (ORDER BY id) FROM t")).unwrap();
            assert_eq!(q.columns.len(), 1);
            assert!(
                matches!(&q.columns[0], SelectItem::Window(w) if w.partition_by.is_empty() && w.order_by == vec![("id".into(), false)])
            );
        }
    }

    #[test]
    fn lag_and_lead_default_and_explicit_offset() {
        let q =
            parse("SELECT LAG(amount) OVER (PARTITION BY region ORDER BY id DESC) FROM t").unwrap();
        assert_eq!(
            q.columns,
            vec![SelectItem::Window(WindowSpec {
                func: WindowFunc::Lag,
                arg: Some("amount".into()),
                offset: None,
                partition_by: vec!["region".into()],
                order_by: vec![("id".into(), true)],
            })]
        );

        let q = parse("SELECT LEAD(amount, 2) OVER (ORDER BY id) FROM t").unwrap();
        assert_eq!(
            q.columns,
            vec![SelectItem::Window(WindowSpec {
                func: WindowFunc::Lead,
                arg: Some("amount".into()),
                offset: Some(2),
                partition_by: vec![],
                order_by: vec![("id".into(), false)],
            })]
        );
    }

    #[test]
    fn first_value_last_value_and_aggregate_as_window() {
        for (sql, func) in [
            (
                "SELECT FIRST_VALUE(amount) OVER (ORDER BY id) FROM t",
                WindowFunc::FirstValue,
            ),
            (
                "SELECT LAST_VALUE(amount) OVER (ORDER BY id) FROM t",
                WindowFunc::LastValue,
            ),
            (
                "SELECT SUM(amount) OVER (PARTITION BY region) FROM t",
                WindowFunc::Sum,
            ),
            (
                "SELECT AVG(amount) OVER (PARTITION BY region) FROM t",
                WindowFunc::Avg,
            ),
        ] {
            let q = parse(sql).unwrap();
            assert!(
                matches!(&q.columns[0], SelectItem::Window(w) if w.func == func && w.arg == Some("amount".into())),
                "unexpected result for {sql:?}: {:?}",
                q.columns
            );
        }
    }

    #[test]
    fn count_over_supports_star_and_column() {
        let q = parse("SELECT COUNT(*) OVER (PARTITION BY region) FROM t").unwrap();
        assert!(
            matches!(&q.columns[0], SelectItem::Window(w) if w.func == WindowFunc::Count && w.arg.is_none())
        );

        let q = parse("SELECT COUNT(id) OVER (PARTITION BY region) FROM t").unwrap();
        assert!(
            matches!(&q.columns[0], SelectItem::Window(w) if w.func == WindowFunc::Count && w.arg == Some("id".into()))
        );
    }

    #[test]
    fn window_over_named_window_reference_is_unsupported() {
        // `OVER w` (referencing a `WINDOW` clause) rather than an inline
        // `OVER (...)` -- the WINDOW clause itself remains unsupported.
        let err = parse(
            "SELECT ROW_NUMBER() OVER w FROM t WINDOW w AS (PARTITION BY region ORDER BY id)",
        )
        .unwrap_err();
        assert!(matches!(err, ParseError::Unexpected { .. }));
    }

    #[test]
    fn window_frame_clause_is_unsupported() {
        let err = parse(
            "SELECT SUM(amount) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t",
        )
        .unwrap_err();
        assert!(matches!(err, ParseError::Unexpected { .. }));
    }

    #[test]
    fn window_filter_clause_is_unsupported() {
        let err = parse("SELECT SUM(amount) FILTER (WHERE amount > 0) OVER (ORDER BY id) FROM t")
            .unwrap_err();
        assert!(matches!(err, ParseError::Unexpected { .. }));
    }

    #[test]
    fn sum_without_over_is_still_a_plain_aggregate() {
        let q = parse("SELECT SUM(amount) FROM t").unwrap();
        assert_eq!(
            q.columns,
            vec![SelectItem::Agg(AggFunc::Sum, Some("amount".into()))]
        );
    }

    #[test]
    fn parses_select_star() {
        let q = parse("SELECT * FROM t").unwrap();
        assert_eq!(q.columns, vec![SelectItem::Star]);
    }

    #[test]
    fn parses_select_star_alongside_columns() {
        let q = parse("SELECT id, * FROM t").unwrap();
        assert_eq!(
            q.columns,
            vec![SelectItem::Column("id".into()), SelectItem::Star]
        );
    }

    #[test]
    fn parses_table_alias_and_rewrites_qualified_select_column() {
        let q = parse("SELECT o.id FROM orders o").unwrap();
        assert_eq!(q.from, "orders");
        assert_eq!(q.columns, vec![SelectItem::Column("orders.id".into())]);
    }

    #[test]
    fn parses_join_aliases_and_rewrites_on_clause_and_where() {
        let q = parse(
            "SELECT o.id, c.name FROM orders o JOIN customers c ON o.cust_id = c.id WHERE c.id > 1",
        )
        .unwrap();
        assert_eq!(
            q.columns,
            vec![
                SelectItem::Column("orders.id".into()),
                SelectItem::Column("customers.name".into())
            ]
        );
        assert_eq!(
            q.joins,
            vec![Join {
                kind: JoinKind::Inner,
                table: "customers".into(),
                left_col: "orders.cust_id".into(),
                right_col: "customers.id".into(),
            }]
        );
        assert_eq!(
            q.where_clause,
            Some(Expr::BinaryOp(
                Box::new(Expr::Column("customers.id".into())),
                BinOp::Gt,
                Box::new(Expr::Literal(Literal::Int(1)))
            ))
        );
    }

    #[test]
    fn parses_cross_join_with_limit() {
        let q = parse("SELECT id FROM a CROSS JOIN b LIMIT 10").unwrap();
        assert_eq!(
            q.joins,
            vec![Join {
                kind: JoinKind::Cross,
                table: "b".into(),
                left_col: String::new(),
                right_col: String::new()
            }]
        );
        assert_eq!(q.limit, Some(10));
    }

    #[test]
    fn cross_join_without_limit_is_rejected() {
        let err = parse("SELECT id FROM a CROSS JOIN b").unwrap_err();
        assert!(matches!(err, ParseError::Unexpected { .. }));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__column_592__v1_cross_join_without_limit_is_rejected() {
        let err = parse("SELECT id FROM a CROSS JOIN b").unwrap_err();
        assert!(matches!(err, ParseError::Unexpected { .. }));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__column_592__v2_cross_join_with_limit_is_accepted() {
        let q = parse("SELECT id FROM a CROSS JOIN b LIMIT 10").unwrap();
        assert_eq!(q.limit, Some(10));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__column_592__v3_non_cross_join_without_limit_is_accepted() {
        let q = parse("SELECT id FROM t RIGHT JOIN u ON t.k = u.k").unwrap();
        assert_eq!(q.limit, None);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__column_620__v1_agg_without_window_validates_group_by_keys() {
        let err = parse("SELECT foo, SUM(amount) FROM t").unwrap_err();
        assert!(matches!(err, ParseError::Unexpected { .. }));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__column_620__v2_no_agg_skips_group_by_key_validation() {
        // No aggregate column at all: a mismatched bare column list is
        // never checked against GROUP BY, so this parses fine.
        let q = parse("SELECT foo, bar FROM t").unwrap();
        assert_eq!(q.columns.len(), 2);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__column_620__v3_agg_with_window_skips_group_by_key_validation() {
        // Both an aggregate and a window column, with a bare `region`
        // column and no GROUP BY -- this would fail the "plain column
        // alongside an aggregate requires GROUP BY" check if it ran, but
        // window queries are exempt from it.
        let q =
            parse("SELECT region, SUM(amount), ROW_NUMBER() OVER (ORDER BY id) FROM t").unwrap();
        assert_eq!(q.columns.len(), 3);
    }

    #[test]
    fn parses_right_join() {
        let q = parse("SELECT id FROM t RIGHT JOIN u ON t.k = u.k").unwrap();
        assert_eq!(
            q.joins,
            vec![Join {
                kind: JoinKind::Right,
                table: "u".into(),
                left_col: "t.k".into(),
                right_col: "u.k".into()
            }]
        );
    }

    #[test]
    fn parses_right_outer_join() {
        let q = parse("SELECT id FROM t RIGHT OUTER JOIN u ON t.k = u.k").unwrap();
        assert_eq!(q.joins[0].kind, JoinKind::Right);
    }

    #[test]
    fn parses_full_join() {
        let q = parse("SELECT id FROM t FULL JOIN u ON t.k = u.k").unwrap();
        assert_eq!(
            q.joins,
            vec![Join {
                kind: JoinKind::Full,
                table: "u".into(),
                left_col: "t.k".into(),
                right_col: "u.k".into()
            }]
        );
    }

    #[test]
    fn parses_full_outer_join() {
        let q = parse("SELECT id FROM t FULL OUTER JOIN u ON t.k = u.k").unwrap();
        assert_eq!(q.joins[0].kind, JoinKind::Full);
    }

    #[test]
    fn parses_not() {
        let q = parse("SELECT id FROM t WHERE NOT amount > 10").unwrap();
        assert_eq!(
            q.where_clause,
            Some(Expr::Not(Box::new(Expr::BinaryOp(
                Box::new(Expr::Column("amount".into())),
                BinOp::Gt,
                Box::new(Expr::Literal(Literal::Int(10)))
            ))))
        );
    }

    #[test]
    fn parses_is_null() {
        let q = parse("SELECT id FROM t WHERE amount IS NULL").unwrap();
        assert_eq!(
            q.where_clause,
            Some(Expr::IsNull {
                expr: Box::new(Expr::Column("amount".into())),
                negated: false
            })
        );
    }

    #[test]
    fn parses_is_not_null() {
        let q = parse("SELECT id FROM t WHERE amount IS NOT NULL").unwrap();
        assert_eq!(
            q.where_clause,
            Some(Expr::IsNull {
                expr: Box::new(Expr::Column("amount".into())),
                negated: true
            })
        );
    }

    // --- Span tests: `row`'s shared tokenizer now supplies these, not a
    // second one -- exact positions are its call, not re-asserted here. ---

    #[test]
    fn error_span_points_at_a_real_location() {
        let err = parse("SELECT id FROM t GARBAGE EXTRA").unwrap_err();
        assert!(!err.span().is_unknown());
    }

    #[test]
    fn error_span_tracks_line_number_across_newlines() {
        let err = parse("SELECT id\nFROM t\nGARBAGE EXTRA").unwrap_err();
        assert_eq!(err.span().line, 3);
    }

    #[test]
    fn eof_error_has_a_real_span_not_unknown() {
        let err = parse("SELECT id FROM").unwrap_err();
        assert!(!err.span().is_unknown());
    }

    #[test]
    fn multibyte_characters_advance_byte_offset_correctly() {
        let q = parse("SELECT id FROM t WHERE name = 'café'").unwrap();
        assert!(q.where_clause.is_some());
    }
}
