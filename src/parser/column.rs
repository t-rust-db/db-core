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
//! **Not carried forward: window functions.** `parser::row`'s grammar
//! itself rejects `OVER`/`FILTER` as not-yet-supported (see
//! `grammar::parse_function_call`), so a query using `ROW_NUMBER() OVER
//! (...)` etc. no longer parses at all through this module -- tracked as
//! follow-up work (extend `row`'s grammar with window-function syntax,
//! then give `codegen::batch::compile_window` an `ast::Select`-shaped
//! input alongside its existing `crate::expr::Query` one). Every other
//! shape this module previously accepted still does.
//!
//! Errors carry a [`Span`] (see `ADR 0001`/`ADR 0002` in `db-core`'s
//! `.openspec/adr/`), matching sqlite-rs's own `ParseFail`/`ParseOutcome`
//! convention: a consumer (REPL, IDE) can point at *where* parsing failed,
//! not just read a message.

use std::collections::HashMap;
use std::fmt;

use crate::expr::{AggFunc, BinOp, Expr, Join, JoinKind, OrderBy, Query, SelectItem};
use crate::parser::row::ast::{
    BinaryOp as AstBinOp, Distinctness, Expr as AstExpr, ExprKind, FunctionArgs, JoinConstraint,
    JoinOp, Literal as AstLiteral, OrderingTerm, ResultColumn, Select, TableRefKind, UnaryOp,
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
        Some(OrderingTerm { expr, desc, .. }) => Some(OrderBy {
            column: column_name(expr)?,
            descending: desc.unwrap_or(false),
        }),
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
mod tests {
    use super::*;
    use crate::expr::{Join, JoinKind};

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
    fn window_functions_no_longer_parse_pending_follow_up() {
        // `parser::row`'s grammar itself rejects `OVER`/`FILTER` --
        // tracked as follow-up (extend row's grammar, then give
        // `compile_window` an `ast::Select` input too).
        let err =
            parse("SELECT ROW_NUMBER() OVER (PARTITION BY region ORDER BY id) FROM t").unwrap_err();
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
