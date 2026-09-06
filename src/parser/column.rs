//! column-rs's section (#27, #57, #63, #65, #67-70): `SELECT ... FROM ...
//! [[INNER|LEFT] JOIN table ON col = col ...] [WHERE ... | WHERE col IN
//! (SELECT ...)] [GROUP BY ...] [ORDER BY ...] [LIMIT ...]`, restricted to
//! the analytics subset the query VM executes. Joins are equi-joins only.
//! The only subquery form is `col IN (SELECT ...)` (a semi-join) as the
//! *entire* `WHERE` clause -- it can't be combined with other conditions
//! via `AND`/`OR`.
//!
//! **Unified on `parser::row`'s tokenizer and grammar (#57), and on its AST
//! (#153).** This module no longer has its own tokenizer, recursive-
//! descent parser, or lowered AST: [`parse`]/[`parse_explain`] parse with
//! [`super::row::parse_select`]/[`super::row::parse_explain`] (sqlite-rs's
//! own, shared with `row`) and return the resulting
//! [`super::ast::Select`] unchanged in shape -- [`crate::codegen::batch`]
//! and `crate::emit::batch` consume it directly, the same AST
//! [`crate::codegen::row`] already does. [`validate_select`] is where
//! "column's grammar becomes an enforced subset" (ADR 0002's second
//! amendment) actually happens: a `Select` outside this subset (a real
//! JOIN condition shape, `WITH`, `UNION`, `HAVING`, non-integer `LIMIT`, a
//! second `ORDER BY` term, ...) is rejected here with
//! [`ParseError::Unexpected`], not silently miscompiled -- and, as a side
//! effect, table aliases (`FROM orders o`) are resolved in place, rewriting
//! every alias-qualified column reference to the real table name, so
//! `codegen::batch` never has to know aliases existed.
//!
//! **Window functions** (#74 follow-up): `parser::row`'s grammar parses
//! `func(...) OVER (PARTITION BY ... ORDER BY ...)` (see
//! `grammar::parse_function_call`/`window_def`); [`validate_window_call`]
//! resolves `func` against its own local name table ([`window_shape`] --
//! deliberately not `crate::codegen::batch::WindowFunc`, so this
//! validator doesn't depend on the planner) and validates its
//! argument/partition/order shape, the same subset
//! `codegen::batch::compile_window` executes. Not carried forward from
//! real SQLite/DuckDB syntax: a named `OVER window_name` (would need the
//! still-unsupported `WINDOW` clause) and an explicit frame (`ROWS`/
//! `RANGE`/`GROUPS ...`, no representation in `ast::WindowDef`) are both
//! rejected with a clear error, not silently accepted or misconverted.
//!
//! Errors carry a [`Span`] (see `ADR 0001`/`ADR 0002` in `db-core`'s
//! `.openspec/adr/`), matching sqlite-rs's own `ParseFail`/`ParseOutcome`
//! convention: a consumer (REPL, IDE) can point at *where* parsing failed,
//! not just read a message.

use std::collections::HashMap;
use std::fmt;

use crate::parser::ast::{
    BinaryOp as AstBinOp, Expr as AstExpr, ExprKind, FunctionArgs, JoinConstraint, JoinOp,
    Literal as AstLiteral, Select, TableRefKind, UnaryOp, WindowDef,
};
use crate::parser::row::ParseOutcome;
use crate::parser::Span;

#[derive(Debug, PartialEq)]
/// Errors from the column-oriented (`codegen-batch`) query validator/parser.
pub enum ParseError {
    /// The query ended before a complete statement was read.
    UnexpectedEof {
        /// Where the input ended.
        span: Span,
    },
    /// A token or construct that is not accepted at this position.
    Unexpected {
        /// What was expected or why the token is rejected.
        message: String,
        /// Where the offending token is.
        span: Span,
    },
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

/// Result alias for this parser, with [`ParseError`] as the error type.
pub type Result<T> = std::result::Result<T, ParseError>;

fn unsupported(span: Span, message: String) -> ParseError {
    ParseError::Unexpected { message, span }
}

/// A (possibly qualified) column reference: `col` or `table.col`. Aliases
/// aren't resolved here -- that happens afterward, via
/// [`resolve_expr_aliases`].
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

fn ast_binop_allowed(op: AstBinOp, span: Span) -> Result<()> {
    match op {
        AstBinOp::Add
        | AstBinOp::Sub
        | AstBinOp::Mul
        | AstBinOp::Div
        | AstBinOp::Eq
        | AstBinOp::Ne
        | AstBinOp::Lt
        | AstBinOp::Le
        | AstBinOp::Gt
        | AstBinOp::Ge
        | AstBinOp::And
        | AstBinOp::Or
        | AstBinOp::Concat => Ok(()),
        other => Err(unsupported(span, format!("operator {other:?}"))),
    }
}

/// Rewrite `alias.col` to `real_table.col` in place, for every qualified
/// column name reachable from `expr` (recursing through every operator
/// this subset's grammar can produce). A subquery has its own `FROM`/alias
/// scope -- its column refs are resolved when *it* is validated, not
/// against the outer query's aliases.
fn resolve_expr_aliases(expr: &mut AstExpr, aliases: &HashMap<String, String>) {
    match &mut expr.kind {
        ExprKind::Column { table, .. } => {
            if let Some(t) = table {
                if let Some(real) = aliases.get(t) {
                    *t = real.clone();
                }
            }
        }
        ExprKind::FunctionCall { args, over, .. } => {
            if let FunctionArgs::List(list) = args {
                for e in list {
                    resolve_expr_aliases(e, aliases);
                }
            }
            if let Some(window_def) = over {
                for e in &mut window_def.partition_by {
                    resolve_expr_aliases(e, aliases);
                }
                for term in &mut window_def.order_by {
                    resolve_expr_aliases(&mut term.expr, aliases);
                }
            }
        }
        ExprKind::Unary { expr: inner, .. } => resolve_expr_aliases(inner, aliases),
        ExprKind::Binary { lhs, rhs, .. } | ExprKind::Is { lhs, rhs, .. } => {
            resolve_expr_aliases(lhs, aliases);
            resolve_expr_aliases(rhs, aliases);
        }
        ExprKind::IsNull { expr: inner, .. } => resolve_expr_aliases(inner, aliases),
        ExprKind::Paren(inner) => resolve_expr_aliases(inner, aliases),
        ExprKind::InSubquery { expr: inner, .. } => resolve_expr_aliases(inner, aliases),
        // Not part of the batch subset (rejected by `validate_expr`
        // before or regardless of alias resolution), but walked here too
        // so a `Select` this module validates is never left half-resolved.
        ExprKind::Between {
            expr: inner,
            lo,
            hi,
            ..
        } => {
            resolve_expr_aliases(inner, aliases);
            resolve_expr_aliases(lo, aliases);
            resolve_expr_aliases(hi, aliases);
        }
        ExprKind::In {
            expr: inner, list, ..
        } => {
            resolve_expr_aliases(inner, aliases);
            for e in list {
                resolve_expr_aliases(e, aliases);
            }
        }
        ExprKind::Like {
            expr: inner,
            pattern,
            escape,
            ..
        } => {
            resolve_expr_aliases(inner, aliases);
            resolve_expr_aliases(pattern, aliases);
            if let Some(e) = escape {
                resolve_expr_aliases(e, aliases);
            }
        }
        ExprKind::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(o) = operand {
                resolve_expr_aliases(o, aliases);
            }
            for (cond, result) in whens {
                resolve_expr_aliases(cond, aliases);
                resolve_expr_aliases(result, aliases);
            }
            if let Some(e) = else_ {
                resolve_expr_aliases(e, aliases);
            }
        }
        ExprKind::Cast { expr: inner, .. } | ExprKind::Collate { expr: inner, .. } => {
            resolve_expr_aliases(inner, aliases)
        }
        ExprKind::Literal(_)
        | ExprKind::Param(_)
        | ExprKind::Subquery(_)
        | ExprKind::Exists { .. }
        | ExprKind::InSubqueryMulti { .. } => {}
    }
}

/// Extract `(left_col, right_col)` from a `JOIN ... ON <expr>` condition.
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

/// The batch planner's known aggregate function names -- a local,
/// name-only list (not `vm::batch::AggFunc`) so this validator doesn't
/// pull in the VM as a dependency; `codegen::batch` re-resolves the same
/// names into its own `AggFunc` independently.
fn is_known_agg_name(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "COUNT" | "SUM" | "AVG" | "MIN" | "MAX"
    )
}

/// Validates an aggregate `FunctionCall` (`COUNT(x)`, `COUNT(*)`, ...):
/// a known aggregate name, and exactly one column or `(*)` (`COUNT` only).
fn validate_aggregate_call(expr: &AstExpr, name: &str, args: &FunctionArgs) -> Result<()> {
    if !is_known_agg_name(name) {
        return Err(unsupported(expr.span, format!("unknown function {name}")));
    }
    match args {
        FunctionArgs::Star => {
            if !name.eq_ignore_ascii_case("COUNT") {
                return Err(unsupported(expr.span, "only COUNT supports (*)".into()));
            }
            Ok(())
        }
        FunctionArgs::List(list) => match list.as_slice() {
            [one] => column_name(one).map(|_| ()),
            _ => Err(unsupported(
                expr.span,
                "an aggregate takes exactly one column or *".into(),
            )),
        },
    }
}

/// A window function name's argument shape -- deliberately a local,
/// name-only classification (not `codegen::batch::WindowFunc`) so this
/// validator doesn't pull in the planner as a dependency; `codegen::batch`
/// re-resolves the same names into its own `WindowFunc` independently.
enum WindowShape {
    /// `ROW_NUMBER`/`RANK`/`DENSE_RANK`: no arguments.
    Niladic,
    /// `LAG`/`LEAD`: one column plus an optional integer offset.
    LagLead,
    /// `COUNT`: one column or `(*)`.
    CountLike,
    /// `SUM`/`AVG`/`FIRST_VALUE`/`LAST_VALUE`: exactly one column.
    OneArg,
}

fn window_shape(name: &str) -> Option<WindowShape> {
    match name.to_ascii_uppercase().as_str() {
        "ROW_NUMBER" | "RANK" | "DENSE_RANK" => Some(WindowShape::Niladic),
        "LAG" | "LEAD" => Some(WindowShape::LagLead),
        "COUNT" => Some(WindowShape::CountLike),
        "SUM" | "AVG" | "FIRST_VALUE" | "LAST_VALUE" => Some(WindowShape::OneArg),
        _ => None,
    }
}

/// Validates `name(args) OVER (window_def)`: resolves `name` against
/// [`window_shape`], validates the argument count/shape each function
/// kind expects (niladic for `ROW_NUMBER`/`RANK`/`DENSE_RANK`, one column
/// plus an optional integer offset for `LAG`/`LEAD`, one column or
/// `COUNT(*)` for the rest), and validates `window_def`'s `PARTITION
/// BY`/`ORDER BY` expressions are plain column references -- the same
/// "enforced subset" restriction the rest of this module applies (no
/// expressions, only column references).
fn validate_window_call(
    span: Span,
    name: &str,
    args: &FunctionArgs,
    window_def: &WindowDef,
) -> Result<()> {
    let shape = window_shape(name)
        .ok_or_else(|| unsupported(span, format!("unknown window function {name}")))?;

    match (&shape, args) {
        (WindowShape::Niladic, FunctionArgs::List(list)) if list.is_empty() => {}
        (WindowShape::Niladic, _) => {
            return Err(unsupported(span, format!("{name} takes no arguments")))
        }
        (WindowShape::CountLike, FunctionArgs::Star) => {}
        (_, FunctionArgs::Star) => return Err(unsupported(span, "only COUNT supports (*)".into())),
        (WindowShape::LagLead, FunctionArgs::List(list)) => match list.as_slice() {
            [one] => {
                column_name(one)?;
            }
            [one, offset_expr] => {
                column_name(one)?;
                if !matches!(offset_expr.kind, ExprKind::Literal(AstLiteral::Integer(_))) {
                    return Err(unsupported(offset_expr.span, "non-integer offset".into()));
                }
            }
            _ => return Err(unsupported(span, format!("{name} takes 1 or 2 arguments"))),
        },
        (_, FunctionArgs::List(list)) => match list.as_slice() {
            [one] => {
                column_name(one)?;
            }
            _ => {
                return Err(unsupported(
                    span,
                    "a window function takes exactly one column or *".into(),
                ))
            }
        },
    }

    for e in &window_def.partition_by {
        column_name(e)?;
    }
    for term in &window_def.order_by {
        column_name(&term.expr)?;
        if term.nulls_last.is_some() {
            return Err(unsupported(span, "NULLS FIRST/LAST".into()));
        }
    }
    Ok(())
}

fn validate_result_column(col: &crate::parser::ast::ResultColumn) -> Result<()> {
    use crate::parser::ast::ResultColumn;
    match col {
        ResultColumn::Star => Ok(()),
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
            ExprKind::Column { .. } => column_name(expr).map(|_| ()),
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
                validate_window_call(expr.span, name, args, window_def)
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
                validate_aggregate_call(expr, name, args)
            }
            _ => Err(unsupported(
                expr.span,
                "unsupported SELECT expression".into(),
            )),
        },
    }
}

/// A `SELECT`-list item's classification, just enough to check the
/// "bare columns must match GROUP BY keys" rule below.
enum ItemKind {
    Column(String),
    Star,
    Agg,
    Window,
}

fn item_kind(col: &crate::parser::ast::ResultColumn) -> ItemKind {
    use crate::parser::ast::ResultColumn;
    match col {
        ResultColumn::Star | ResultColumn::TableStar { .. } => ItemKind::Star,
        ResultColumn::Expr { expr, .. } => match &expr.kind {
            ExprKind::Column { .. } => ItemKind::Column(column_name(expr).unwrap_or_default()),
            ExprKind::FunctionCall { over: Some(_), .. } => ItemKind::Window,
            ExprKind::FunctionCall { over: None, .. } => ItemKind::Agg,
            _ => ItemKind::Column(String::new()),
        },
    }
}

fn validate_expr(expr: &mut AstExpr) -> Result<()> {
    match &mut expr.kind {
        ExprKind::Literal(_) => Ok(()),
        ExprKind::Column {
            catalog: Some(_), ..
        } => Err(unsupported(expr.span, "catalog-qualified column".into())),
        ExprKind::Column { .. } => Ok(()),
        ExprKind::Unary {
            op: UnaryOp::Not | UnaryOp::Minus | UnaryOp::Plus,
            expr: inner,
        } => validate_expr(inner),
        ExprKind::Unary { op, .. } => Err(unsupported(expr.span, format!("unary operator {op:?}"))),
        // `expr IS [NOT] NULL` may parse as `Is{lhs, rhs: NULL literal,
        // negated}` instead of the dedicated `IsNull` node.
        ExprKind::Is { lhs, rhs, .. }
            if matches!(rhs.kind, ExprKind::Literal(AstLiteral::Null)) =>
        {
            validate_expr(lhs)
        }
        ExprKind::Binary { op, lhs, rhs } => {
            ast_binop_allowed(*op, expr.span)?;
            validate_expr(lhs)?;
            validate_expr(rhs)
        }
        ExprKind::IsNull { expr: inner, .. } => validate_expr(inner),
        ExprKind::Paren(inner) => validate_expr(inner),
        ExprKind::InSubquery {
            expr: inner,
            subquery,
            negated: false,
        } => {
            validate_expr(inner)?;
            validate_select(subquery)
        }
        ExprKind::InSubquery { negated: true, .. } => {
            Err(unsupported(expr.span, "NOT IN (SELECT ...)".into()))
        }
        ExprKind::Exists { subquery, .. } => validate_select(subquery),
        other => Err(unsupported(
            expr.span,
            format!("unsupported expression form {other:?}"),
        )),
    }
}

/// Validates a `Select` parsed by [`super::row`]'s shared grammar against
/// column-rs's analytics subset, rejecting anything outside it with
/// [`ParseError::Unexpected`] -- ADR 0002's "column's grammar becomes an
/// enforced subset" (parsing succeeds, validation declines), not a second
/// parser that can't parse these constructs at all. On success, mutates
/// `select` in place to resolve every table-alias-qualified column
/// reference to the real table name (see [`resolve_expr_aliases`]), so
/// [`crate::codegen::batch`] never has to know aliases existed.
fn validate_select(select: &mut Select) -> Result<()> {
    if select.with_clause.is_some() {
        return Err(unsupported(select.span, "WITH clause".into()));
    }
    if !select.compound.is_empty() {
        return Err(unsupported(select.span, "UNION".into()));
    }
    if select.having.is_some() {
        return Err(unsupported(select.span, "HAVING".into()));
    }

    let mut aliases: HashMap<String, String> = HashMap::new();
    let from_name;
    let mut has_cross_join = false;
    {
        let Some(from_clause) = select.from.as_mut() else {
            return Err(unsupported(select.span, "SELECT without FROM".into()));
        };
        // db-core#95: a `FROM`-subquery's mandatory alias is the name the
        // enclosing query refers to it by, so it takes `from_name`'s place
        // in the alias table below.
        from_name = match &mut from_clause.first.kind {
            TableRefKind::Name(name) => name.clone(),
            TableRefKind::Subquery(subselect) => {
                let Some(alias) = from_clause.first.alias.clone() else {
                    return Err(unsupported(
                        from_clause.first.span,
                        "a subquery in FROM requires an alias".into(),
                    ));
                };
                validate_select(subselect)?;
                alias
            }
        };
        if let Some(alias) = &from_clause.first.alias {
            aliases.insert(alias.clone(), from_name.clone());
        }

        for j in &mut from_clause.joins {
            if j.natural {
                return Err(unsupported(j.table.span, "NATURAL join".into()));
            }
            let table = match &j.table.kind {
                TableRefKind::Name(table) => table.clone(),
                TableRefKind::Subquery(_) => {
                    return Err(unsupported(j.table.span, "subquery in JOIN".into()))
                }
            };
            if let Some(alias) = &j.table.alias {
                aliases.insert(alias.clone(), table.clone());
            }
            if j.op == JoinOp::Cross {
                has_cross_join = true;
            }
            match &j.constraint {
                Some(JoinConstraint::On(expr)) => {
                    extract_equi_join(expr)?;
                }
                Some(JoinConstraint::Using(_)) => {
                    return Err(unsupported(j.table.span, "USING join".into()))
                }
                None if j.op == JoinOp::Cross => {}
                None => return Err(unsupported(j.table.span, "join without ON".into())),
            }
        }
    }

    if !aliases.is_empty() {
        if let Some(where_clause) = &mut select.where_clause {
            resolve_expr_aliases(where_clause, &aliases);
        }
        for col in &mut select.columns {
            if let crate::parser::ast::ResultColumn::Expr { expr, .. } = col {
                resolve_expr_aliases(expr, &aliases);
            }
        }
        for e in &mut select.group_by {
            resolve_expr_aliases(e, &aliases);
        }
        for term in &mut select.order_by {
            resolve_expr_aliases(&mut term.expr, &aliases);
        }
        if let Some(from_clause) = select.from.as_mut() {
            for j in &mut from_clause.joins {
                if let Some(JoinConstraint::On(expr)) = &mut j.constraint {
                    resolve_expr_aliases(expr, &aliases);
                }
            }
        }
    }

    if let Some(where_clause) = &mut select.where_clause {
        validate_expr(where_clause)?;
    }

    for e in &select.group_by {
        column_name(e)?;
    }

    if select.order_by.len() > 1 {
        return Err(unsupported(select.span, "multiple ORDER BY terms".into()));
    }
    if let Some(term) = select.order_by.first() {
        if term.nulls_last.is_some() {
            return Err(unsupported(select.span, "NULLS FIRST/LAST".into()));
        }
        match &term.expr.kind {
            ExprKind::Column { .. } => {
                column_name(&term.expr)?;
            }
            ExprKind::FunctionCall {
                name,
                distinct,
                args,
                over: None,
            } => {
                if *distinct {
                    return Err(unsupported(
                        term.expr.span,
                        "DISTINCT inside an aggregate".into(),
                    ));
                }
                validate_aggregate_call(&term.expr, name, args)?;
            }
            _ => {
                return Err(unsupported(
                    term.expr.span,
                    format!(
                        "expected a column reference or aggregate, found {:?}",
                        term.expr.kind
                    ),
                ))
            }
        }
    }

    let limit_ok = match &select.limit {
        Some(l) => matches!(&l.limit.kind, ExprKind::Literal(AstLiteral::Integer(n)) if *n >= 0),
        None => true,
    };
    if let Some(l) = &select.limit {
        if !limit_ok {
            return Err(unsupported(l.limit.span, "non-integer LIMIT".into()));
        }
        if let Some(offset) = &l.offset {
            if !matches!(&offset.kind, ExprKind::Literal(AstLiteral::Integer(n)) if *n >= 0) {
                return Err(unsupported(offset.span, "non-integer OFFSET".into()));
            }
        }
    }

    if has_cross_join && select.limit.is_none() {
        return Err(unsupported(
            select.span,
            "CROSS JOIN requires a LIMIT (bounded-execution rule -- an unconditional cross \
             product has no natural row cap)"
                .into(),
        ));
    }

    for col in &select.columns {
        validate_result_column(col)?;
    }

    // `codegen::batch::compile` emits every non-aggregated SELECT column
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
    let items: Vec<ItemKind> = select.columns.iter().map(item_kind).collect();
    let has_window = items.iter().any(|c| matches!(c, ItemKind::Window));
    let has_agg = items.iter().any(|c| matches!(c, ItemKind::Agg));
    if has_agg && !has_window {
        let select_bare: Vec<&String> = items
            .iter()
            .filter_map(|c| match c {
                ItemKind::Column(name) => Some(name),
                _ => None,
            })
            .collect();
        let group_by: Vec<String> = select
            .group_by
            .iter()
            .filter_map(|e| column_name(e).ok())
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

    Ok(())
}

fn from_outcome(message: String, span: Span) -> ParseError {
    ParseError::Unexpected { message, span }
}

/// Parses `sql_text`, validating it against column-rs's analytics subset
/// and resolving table aliases in place. The returned [`Select`] is the
/// same AST [`crate::codegen::row`] consumes -- there is one AST (#153).
pub fn parse(sql_text: &str) -> Result<Select> {
    match crate::parser::row::parse_select(sql_text) {
        ParseOutcome::Accepted(mut select) => {
            validate_select(&mut select)?;
            Ok(*select)
        }
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
/// (if any) prefixed it along with the parsed/validated query. The
/// distinction (#55) falls out of unifying on `row`'s grammar for free --
/// its `parse_explain_stmt` already tracks bare `EXPLAIN` vs `EXPLAIN
/// QUERY PLAN` via `ast::Explain::query_plan`.
pub fn parse_explain(sql_text: &str) -> Result<(Explain, Select)> {
    let starts_with_explain = sql_text
        .split_whitespace()
        .next()
        .is_some_and(|w| w.eq_ignore_ascii_case("EXPLAIN"));
    if !starts_with_explain {
        return Ok((Explain::None, parse(sql_text)?));
    }
    match crate::parser::row::parse_explain(sql_text) {
        ParseOutcome::Accepted(explain) => {
            let form = if explain.query_plan {
                Explain::QueryPlan
            } else {
                Explain::Opcodes
            };
            let mut select = *explain.select;
            validate_select(&mut select)?;
            Ok((form, select))
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
    use crate::parser::ast::ResultColumn;

    /// Test-only helper: the bare (possibly qualified) column name behind
    /// one `SELECT`-list item, or `None` if it isn't a plain column.
    fn bare_col(rc: &ResultColumn) -> Option<String> {
        match rc {
            ResultColumn::Expr { expr, alias: None }
                if matches!(expr.kind, ExprKind::Column { .. }) =>
            {
                column_name(expr).ok()
            }
            _ => None,
        }
    }

    fn col_names(select: &Select) -> Vec<String> {
        select.columns.iter().filter_map(bare_col).collect()
    }

    fn is_star(rc: &ResultColumn) -> bool {
        matches!(rc, ResultColumn::Star)
    }

    /// The table this `FROM`/subquery-alias resolves to.
    fn from_name(select: &Select) -> &str {
        let from = select.from.as_ref().unwrap();
        match &from.first.kind {
            TableRefKind::Name(name) => name,
            TableRefKind::Subquery(_) => from.first.alias.as_deref().unwrap(),
        }
    }

    fn where_expr(select: &Select) -> &AstExpr {
        select.where_clause.as_ref().unwrap()
    }

    #[test]
    fn parse_explain_distinguishes_opcodes_query_plan_and_none() {
        let (explain, query) = parse_explain("EXPLAIN SELECT id FROM orders").unwrap();
        assert_eq!(explain, Explain::Opcodes);
        assert_eq!(from_name(&query), "orders");

        let (explain, _) = parse_explain("EXPLAIN QUERY PLAN SELECT id FROM orders").unwrap();
        assert_eq!(explain, Explain::QueryPlan);

        let (explain, _) = parse_explain("SELECT id FROM orders").unwrap();
        assert_eq!(explain, Explain::None);
    }

    #[test]
    fn parses_columns_and_where() {
        let q = parse("SELECT id, amount FROM orders WHERE amount > 10").unwrap();
        assert_eq!(col_names(&q), vec!["id".to_string(), "amount".to_string()]);
        assert!(matches!(
            &where_expr(&q).kind,
            ExprKind::Binary {
                op: AstBinOp::Gt,
                ..
            }
        ));
    }

    #[test]
    fn parses_unary_minus() {
        let q = parse("SELECT id FROM orders WHERE amount = -5").unwrap();
        let ExprKind::Binary { op, rhs, .. } = &where_expr(&q).kind else {
            panic!("expected Binary")
        };
        assert_eq!(*op, AstBinOp::Eq);
        assert!(matches!(
            rhs.kind,
            ExprKind::Unary {
                op: UnaryOp::Minus,
                ..
            }
        ));
    }

    #[test]
    fn unary_minus_is_chainable_and_unary_plus_is_a_no_op() {
        let q = parse("SELECT id FROM orders WHERE amount = - -5").unwrap();
        let ExprKind::Binary { rhs, .. } = &where_expr(&q).kind else {
            panic!("expected Binary")
        };
        let ExprKind::Unary {
            op: UnaryOp::Minus,
            expr: inner,
        } = &rhs.kind
        else {
            panic!("expected outer unary minus")
        };
        assert!(matches!(
            inner.kind,
            ExprKind::Unary {
                op: UnaryOp::Minus,
                ..
            }
        ));

        let q = parse("SELECT id FROM orders WHERE amount = +5").unwrap();
        let ExprKind::Binary { rhs, .. } = &where_expr(&q).kind else {
            panic!("expected Binary")
        };
        // Unary `+` is validated as a no-op (accepted, not rejected) but
        // -- unlike the retired `expr::Expr` lowering -- is no longer
        // rewritten away: `codegen::batch::compile_expr` treats it as a
        // pass-through at compile time instead.
        assert!(matches!(
            rhs.kind,
            ExprKind::Unary {
                op: UnaryOp::Plus,
                ..
            }
        ));
    }

    #[test]
    fn unary_minus_binds_tighter_than_multiplication() {
        let q = parse("SELECT id FROM orders WHERE amount = -2 * 3").unwrap();
        let ExprKind::Binary { rhs, .. } = &where_expr(&q).kind else {
            panic!("expected Binary")
        };
        let ExprKind::Binary {
            op: AstBinOp::Mul,
            lhs,
            ..
        } = &rhs.kind
        else {
            panic!("expected Mul")
        };
        assert!(matches!(
            lhs.kind,
            ExprKind::Unary {
                op: UnaryOp::Minus,
                ..
            }
        ));
    }

    #[test]
    fn parses_string_concat() {
        let q = parse("SELECT id FROM orders WHERE name = 'a' || 'b'").unwrap();
        let ExprKind::Binary { rhs, .. } = &where_expr(&q).kind else {
            panic!("expected Binary")
        };
        assert!(matches!(
            rhs.kind,
            ExprKind::Binary {
                op: AstBinOp::Concat,
                ..
            }
        ));
    }

    #[test]
    fn concat_binds_tighter_than_multiplication_matching_sqlite_not_duckdb() {
        // Behavior change from unification (#57): this subset now uses
        // `row`'s (sqlite-rs's) operator precedence, where `||` binds
        // *tighter* than `*`/`/`.
        let q = parse("SELECT id FROM orders WHERE x = 2 * 3 || 'x'").unwrap();
        let ExprKind::Binary { rhs, .. } = &where_expr(&q).kind else {
            panic!("expected Binary")
        };
        let ExprKind::Binary {
            op: AstBinOp::Mul,
            rhs: inner_rhs,
            ..
        } = &rhs.kind
        else {
            panic!("expected Mul at the top")
        };
        assert!(matches!(
            inner_rhs.kind,
            ExprKind::Binary {
                op: AstBinOp::Concat,
                ..
            }
        ));
    }

    #[test]
    fn parses_group_by_aggregate() {
        let q = parse("SELECT region, SUM(amount) FROM t WHERE x > 10 GROUP BY region").unwrap();
        assert_eq!(col_names(&q), vec!["region".to_string()]);
        assert!(matches!(item_kind(&q.columns[1]), ItemKind::Agg));
        assert_eq!(
            q.group_by
                .iter()
                .map(|e| column_name(e).unwrap())
                .collect::<Vec<_>>(),
            vec!["region".to_string()]
        );
    }

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

    #[test]
    fn bare_column_not_a_group_by_key_is_rejected() {
        let err = parse("SELECT active, SUM(amount) FROM t GROUP BY region").unwrap_err();
        assert!(matches!(err, ParseError::Unexpected { .. }));
    }

    #[test]
    fn select_list_group_by_keys_in_a_different_order_than_group_by_is_rejected() {
        let err =
            parse("SELECT year, region, SUM(amount) FROM t GROUP BY region, year").unwrap_err();
        assert!(matches!(err, ParseError::Unexpected { .. }));
    }

    #[test]
    fn group_by_key_omitted_from_select_list_is_rejected() {
        let err = parse("SELECT SUM(amount) FROM t GROUP BY region").unwrap_err();
        assert!(matches!(err, ParseError::Unexpected { .. }));
    }

    #[test]
    fn select_list_matching_group_by_keys_exactly_is_accepted() {
        assert!(parse("SELECT region, year, SUM(amount) FROM t GROUP BY region, year").is_ok());
    }

    #[test]
    fn window_function_alongside_a_bare_column_needs_no_group_by() {
        assert!(
            parse("SELECT id, ROW_NUMBER() OVER (PARTITION BY region ORDER BY id) FROM t").is_ok()
        );
    }

    #[test]
    fn parses_distinct() {
        let q = parse("SELECT DISTINCT a, b FROM t").unwrap();
        assert!(matches!(
            q.distinct,
            Some(crate::parser::ast::Distinctness::Distinct)
        ));
        assert_eq!(col_names(&q), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn plain_select_is_not_distinct() {
        let q = parse("SELECT a FROM t").unwrap();
        assert!(q.distinct.is_none());
    }

    #[test]
    fn parses_order_by_and_limit() {
        let q = parse("SELECT id FROM t ORDER BY id DESC LIMIT 5").unwrap();
        let term = q.order_by.first().unwrap();
        assert_eq!(column_name(&term.expr).unwrap(), "id");
        assert_eq!(term.desc, Some(true));
        assert!(matches!(
            q.limit.as_ref().unwrap().limit.kind,
            ExprKind::Literal(AstLiteral::Integer(5))
        ));
    }

    #[test]
    fn parses_limit_with_offset() {
        let q = parse("SELECT id FROM t LIMIT 5 OFFSET 10").unwrap();
        let limit = q.limit.as_ref().unwrap();
        assert!(matches!(
            limit.limit.kind,
            ExprKind::Literal(AstLiteral::Integer(5))
        ));
        assert!(matches!(
            limit.offset.as_ref().unwrap().kind,
            ExprKind::Literal(AstLiteral::Integer(10))
        ));
    }

    #[test]
    fn a_query_without_offset_lowers_none() {
        let q = parse("SELECT id FROM t LIMIT 5").unwrap();
        assert!(q.limit.as_ref().unwrap().offset.is_none());
    }

    #[test]
    fn order_by_references_a_select_list_aggregate() {
        let q = parse(
            "SELECT customer_id, COUNT(event_id), SUM(amount) FROM events \
             GROUP BY customer_id ORDER BY COUNT(event_id) DESC",
        )
        .unwrap();
        let term = q.order_by.first().unwrap();
        assert!(matches!(
            term.expr.kind,
            ExprKind::FunctionCall { over: None, .. }
        ));
        assert_eq!(term.desc, Some(true));
    }

    #[test]
    fn order_by_references_count_star() {
        let q = parse("SELECT COUNT(*) FROM t ORDER BY COUNT(*)").unwrap();
        let term = q.order_by.first().unwrap();
        assert!(matches!(
            &term.expr.kind,
            ExprKind::FunctionCall {
                args: FunctionArgs::Star,
                over: None,
                ..
            }
        ));
    }

    #[test]
    fn order_by_rejects_non_aggregate_expression() {
        let err = parse("SELECT id FROM t ORDER BY id + 1").unwrap_err();
        assert!(format!("{err}").contains("expected a column reference"));
    }

    #[test]
    fn parses_count_star() {
        let q = parse("SELECT COUNT(*) FROM t").unwrap();
        assert!(matches!(item_kind(&q.columns[0]), ItemKind::Agg));
    }

    #[test]
    fn rejects_trailing_garbage() {
        let err = parse("SELECT id FROM t GARBAGE EXTRA").unwrap_err();
        assert!(matches!(err, ParseError::Unexpected { .. }));
    }

    #[test]
    fn parses_inner_join() {
        let q = parse("SELECT orders.id, customers.name FROM orders JOIN customers ON orders.cust_id = customers.id").unwrap();
        assert_eq!(from_name(&q), "orders");
        assert_eq!(
            col_names(&q),
            vec!["orders.id".to_string(), "customers.name".to_string()]
        );
        let join = &q.from.as_ref().unwrap().joins[0];
        assert_eq!(join.op, JoinOp::Inner);
        assert_eq!(join.table.name(), Some("customers"));
        let Some(JoinConstraint::On(on_expr)) = &join.constraint else {
            panic!("expected ON")
        };
        assert_eq!(
            extract_equi_join(on_expr).unwrap(),
            ("orders.cust_id".to_string(), "customers.id".to_string())
        );
    }

    #[test]
    fn parses_left_join() {
        let q = parse("SELECT id FROM t LEFT JOIN u ON t.k = u.k").unwrap();
        let join = &q.from.as_ref().unwrap().joins[0];
        assert_eq!(join.op, JoinOp::Left);
        assert_eq!(join.table.name(), Some("u"));
    }

    #[test]
    fn parses_in_subquery() {
        let q =
            parse("SELECT id FROM orders WHERE region_key IN (SELECT rkey FROM regions)").unwrap();
        let ExprKind::InSubquery { expr, subquery, .. } = &where_expr(&q).kind else {
            panic!("expected InSubquery")
        };
        assert_eq!(column_name(expr).unwrap(), "region_key");
        assert_eq!(from_name(subquery), "regions");
        assert_eq!(col_names(subquery), vec!["rkey".to_string()]);
    }

    #[test]
    fn row_number_over_partition_and_order_by() {
        let q = parse("SELECT ROW_NUMBER() OVER (PARTITION BY region ORDER BY id) FROM t").unwrap();
        assert!(matches!(item_kind(&q.columns[0]), ItemKind::Window));
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
            assert!(matches!(item_kind(&q.columns[0]), ItemKind::Window));
        }
    }

    #[test]
    fn lag_and_lead_default_and_explicit_offset() {
        assert!(
            parse("SELECT LAG(amount) OVER (PARTITION BY region ORDER BY id DESC) FROM t").is_ok()
        );
        assert!(parse("SELECT LEAD(amount, 2) OVER (ORDER BY id) FROM t").is_ok());
    }

    #[test]
    fn first_value_last_value_and_aggregate_as_window() {
        for sql in [
            "SELECT FIRST_VALUE(amount) OVER (ORDER BY id) FROM t",
            "SELECT LAST_VALUE(amount) OVER (ORDER BY id) FROM t",
            "SELECT SUM(amount) OVER (PARTITION BY region) FROM t",
            "SELECT AVG(amount) OVER (PARTITION BY region) FROM t",
        ] {
            let q = parse(sql).unwrap();
            assert!(
                matches!(item_kind(&q.columns[0]), ItemKind::Window),
                "{sql:?}"
            );
        }
    }

    #[test]
    fn count_over_supports_star_and_column() {
        assert!(parse("SELECT COUNT(*) OVER (PARTITION BY region) FROM t").is_ok());
        assert!(parse("SELECT COUNT(id) OVER (PARTITION BY region) FROM t").is_ok());
    }

    #[test]
    fn window_over_named_window_reference_is_unsupported() {
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
        assert!(matches!(item_kind(&q.columns[0]), ItemKind::Agg));
    }

    #[test]
    fn parses_select_star() {
        let q = parse("SELECT * FROM t").unwrap();
        assert!(is_star(&q.columns[0]));
    }

    #[test]
    fn parses_select_star_alongside_columns() {
        let q = parse("SELECT id, * FROM t").unwrap();
        assert_eq!(col_names(&q), vec!["id".to_string()]);
        assert!(is_star(&q.columns[1]));
    }

    #[test]
    fn parses_table_alias_and_rewrites_qualified_select_column() {
        let q = parse("SELECT o.id FROM orders o").unwrap();
        assert_eq!(from_name(&q), "orders");
        assert_eq!(col_names(&q), vec!["orders.id".to_string()]);
    }

    #[test]
    fn parses_join_aliases_and_rewrites_on_clause_and_where() {
        let q = parse(
            "SELECT o.id, c.name FROM orders o JOIN customers c ON o.cust_id = c.id WHERE c.id > 1",
        )
        .unwrap();
        assert_eq!(
            col_names(&q),
            vec!["orders.id".to_string(), "customers.name".to_string()]
        );
        let join = &q.from.as_ref().unwrap().joins[0];
        assert_eq!(join.table.name(), Some("customers"));
        let Some(JoinConstraint::On(on_expr)) = &join.constraint else {
            panic!("expected ON")
        };
        assert_eq!(
            extract_equi_join(on_expr).unwrap(),
            ("orders.cust_id".to_string(), "customers.id".to_string())
        );
        assert!(matches!(
            &where_expr(&q).kind,
            ExprKind::Binary {
                op: AstBinOp::Gt,
                ..
            }
        ));
        let ExprKind::Binary { lhs, .. } = &where_expr(&q).kind else {
            panic!("expected a Binary WHERE expression")
        };
        assert_eq!(column_name(lhs).unwrap(), "customers.id");
    }

    #[test]
    fn parses_cross_join_with_limit() {
        let q = parse("SELECT id FROM a CROSS JOIN b LIMIT 10").unwrap();
        let join = &q.from.as_ref().unwrap().joins[0];
        assert_eq!(join.op, JoinOp::Cross);
        assert!(matches!(
            q.limit.as_ref().unwrap().limit.kind,
            ExprKind::Literal(AstLiteral::Integer(10))
        ));
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
        assert!(matches!(
            q.limit.as_ref().unwrap().limit.kind,
            ExprKind::Literal(AstLiteral::Integer(10))
        ));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__column_592__v3_non_cross_join_without_limit_is_accepted() {
        let q = parse("SELECT id FROM t RIGHT JOIN u ON t.k = u.k").unwrap();
        assert!(q.limit.is_none());
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
        let q = parse("SELECT foo, bar FROM t").unwrap();
        assert_eq!(q.columns.len(), 2);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__column_620__v3_agg_with_window_skips_group_by_key_validation() {
        let q =
            parse("SELECT region, SUM(amount), ROW_NUMBER() OVER (ORDER BY id) FROM t").unwrap();
        assert_eq!(q.columns.len(), 3);
    }

    #[test]
    fn parses_right_join() {
        let q = parse("SELECT id FROM t RIGHT JOIN u ON t.k = u.k").unwrap();
        assert_eq!(q.from.as_ref().unwrap().joins[0].op, JoinOp::Right);
    }

    #[test]
    fn parses_right_outer_join() {
        let q = parse("SELECT id FROM t RIGHT OUTER JOIN u ON t.k = u.k").unwrap();
        assert_eq!(q.from.as_ref().unwrap().joins[0].op, JoinOp::Right);
    }

    #[test]
    fn parses_full_join() {
        let q = parse("SELECT id FROM t FULL JOIN u ON t.k = u.k").unwrap();
        assert_eq!(q.from.as_ref().unwrap().joins[0].op, JoinOp::Full);
    }

    #[test]
    fn parses_full_outer_join() {
        let q = parse("SELECT id FROM t FULL OUTER JOIN u ON t.k = u.k").unwrap();
        assert_eq!(q.from.as_ref().unwrap().joins[0].op, JoinOp::Full);
    }

    #[test]
    fn parses_not() {
        let q = parse("SELECT id FROM t WHERE NOT amount > 10").unwrap();
        assert!(matches!(
            &where_expr(&q).kind,
            ExprKind::Unary {
                op: UnaryOp::Not,
                ..
            }
        ));
    }

    #[test]
    fn parses_is_null() {
        let q = parse("SELECT id FROM t WHERE amount IS NULL").unwrap();
        match &where_expr(&q).kind {
            ExprKind::IsNull { negated, .. } => assert!(!negated),
            ExprKind::Is { negated, .. } => assert!(!negated),
            other => panic!("expected IsNull-shaped expr, found {other:?}"),
        }
    }

    #[test]
    fn parses_is_not_null() {
        let q = parse("SELECT id FROM t WHERE amount IS NOT NULL").unwrap();
        match &where_expr(&q).kind {
            ExprKind::IsNull { negated, .. } => assert!(negated),
            ExprKind::Is { negated, .. } => assert!(negated),
            other => panic!("expected IsNull-shaped expr, found {other:?}"),
        }
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
