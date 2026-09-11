// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `codegen::stream`: the stream planner (ADR 0018 §Planner).
//!
//! `compile(select, scope)`:
//! 1. **Validates by rejecting** (ADR 0002) after a full parse: one
//!    grammar, one AST; the stream subset is enforced with a
//!    `Span`-carrying error, never by parsing less. v1 rejects joins and
//!    any blocking operator (`ORDER BY` without `LIMIT`, an aggregate)
//!    whose only boundary is [`Scope::All`].
//! 2. Lifts `SINCE`/`UNTIL` (or the default `scope` argument) and `=`
//!    predicates on dictionary columns out of `WHERE` into a [`Prune`];
//!    keeps the residual.
//! 3. Delegates the residual `WHERE`/projection/aggregates to
//!    `codegen::batch::compile` on the rewritten `Select`.
//! 4. Wraps: `Prune` prologue, `Body`.
//!
//! Severity-literal rewriting (`severity >= 'WARN'` -> `severity >= 13`)
//! lives here too (moved from `engine::stream::rewrite`, ADR 0018's
//! intended home for it) since it is exactly the kind of stream-only AST
//! rewrite this module exists to own.

use std::fmt;
use std::time::Duration;

use crate::codegen::batch::{self as batch_planner, PlanError};
use crate::parser::ast::{BinaryOp, Expr, ExprKind, Literal, ResultColumn, ScopeUnit, Select};
use crate::parser::Span;
use crate::storage::stream::Severity;
use crate::vm::batch::Program as BatchProgram;
use crate::vm::stream::{IndexPred, Program, Prune};

pub use crate::vm::stream::Scope;

/// A rejected query (ADR 0002 "validate by rejecting"), or a downstream
/// `codegen::batch` failure once the residual is delegated.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamPlanError {
    /// A construct this planner's v1 subset does not accept, with the
    /// span of the operator that triggered it and the boundary (a
    /// narrower `SINCE`/`UNTIL`/`LIMIT`) that would fix it.
    Rejected {
        /// What was rejected and why.
        message: String,
        /// The span of the offending operator.
        span: Span,
    },
    /// A literal this planner cannot interpret (e.g. an unknown severity
    /// name) -- the query shape is fine, one value in it is not.
    InvalidLiteral {
        /// What was wrong with the literal.
        message: String,
        /// The span of the offending literal.
        span: Span,
    },
    /// Any other error surfaced compiling the residual query.
    Batch(PlanError),
}

impl fmt::Display for StreamPlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected { message, .. } | Self::InvalidLiteral { message, .. } => {
                write!(f, "{message}")
            }
            Self::Batch(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for StreamPlanError {}

impl From<PlanError> for StreamPlanError {
    fn from(e: PlanError) -> Self {
        Self::Batch(e)
    }
}

/// Result alias for [`compile`].
pub type Result<T> = std::result::Result<T, StreamPlanError>;

/// Compile `select` into a stream [`Program`]: a `Prune` prologue
/// (segment selection) plus a `vm::batch::Program` body reused verbatim
/// for the residual query.
///
/// `scope` is the effective boundary for this query — resolved by the
/// caller (query `SINCE`/`UNTIL` > CLI > config > built-in default, ADR
/// 0018 §Scope and retention; `Scope`'s own default lives in the client,
/// not here).
pub fn compile(select: &Select, scope: Scope) -> Result<Program> {
    validate(select, &scope)?;

    let mut select = select.clone();
    rewrite_severity_literals(&mut select)?;

    let mut preds = Vec::new();
    if let Some(clause) = select.scope.take() {
        if let Some(since) = clause.since {
            preds.extend(time_pred(&since, scope_now_ns(), true));
        }
        if let Some(until) = clause.until {
            preds.extend(time_pred(&until, scope_now_ns(), false));
        }
    }
    if let Some(where_clause) = &select.where_clause {
        collect_dict_eq(where_clause, &mut preds);
    }

    let body: BatchProgram = batch_planner::compile(&select)?;
    Ok(Program {
        prune: Prune { scope, preds },
        body,
    })
}

/// v1 reject-based validation (ADR 0002): joins have no plan here (the
/// stream engine has one table), and a blocking operator (`ORDER BY`
/// without `LIMIT`, an aggregate) has no boundary once `scope` is
/// [`Scope::All`] — every row would have to be seen before the first is
/// produced, on a stream with no defined end.
fn validate(select: &Select, scope: &Scope) -> Result<()> {
    if let Some(from) = &select.from {
        if let Some(join) = from.joins.first() {
            return Err(StreamPlanError::Rejected {
                message: "JOIN: the stream engine has one table; scope each side \
                          separately and join in the client"
                    .to_string(),
                span: join.table.span,
            });
        }
    }

    if matches!(scope, Scope::All) {
        if let Some(order) = select.order_by.first() {
            if select.limit.is_none() {
                return Err(StreamPlanError::Rejected {
                    message: "ORDER BY with no LIMIT has no boundary over Scope::All; \
                              add a LIMIT or a narrower SINCE/UNTIL"
                        .to_string(),
                    span: order.expr.span,
                });
            }
        }
        if let Some((expr, span)) = first_aggregate(select) {
            let _ = expr;
            return Err(StreamPlanError::Rejected {
                message: "an aggregate has no boundary over Scope::All; \
                          add a narrower SINCE/UNTIL"
                    .to_string(),
                span,
            });
        }
    }
    Ok(())
}

/// The first aggregate function call in the result-column list, if any.
fn first_aggregate(select: &Select) -> Option<(&Expr, Span)> {
    select.columns.iter().find_map(|c| match c {
        ResultColumn::Expr { expr, .. } => aggregate_in(expr),
        ResultColumn::Star | ResultColumn::TableStar { .. } => None,
    })
}

fn aggregate_in(expr: &Expr) -> Option<(&Expr, Span)> {
    match &expr.kind {
        ExprKind::FunctionCall { name, tail, .. }
            if crate::vm::batch::AggFunc::from_name(name).is_some()
                && !matches!(tail.as_deref(), Some(t) if t.over.is_some()) =>
        {
            Some((expr, expr.span))
        }
        _ => None,
    }
}

/// Copies every top-level `column = 'literal'` conjunct (joined only by
/// `AND`) into an [`IndexPred::DictEq`] for segment pruning -- a skip
/// index only, per ADR 0018's "Indexing: block-level only" (a segment
/// whose dictionary *has* the value may still hold non-matching rows), so
/// the predicate stays in the residual `WHERE` for `codegen::batch` to
/// filter exactly; this pass never removes anything from `expr`, only
/// collects what `storage::stream`'s dictionaries can answer without
/// materializing a row (whether the column actually is a dictionary is
/// checked at segment-selection time in `engine::stream`, not here: this
/// planner has no schema to consult).
fn collect_dict_eq(expr: &Expr, preds: &mut Vec<IndexPred>) {
    match &expr.kind {
        ExprKind::Binary {
            op: BinaryOp::And,
            lhs,
            rhs,
        } => {
            collect_dict_eq(lhs, preds);
            collect_dict_eq(rhs, preds);
        }
        ExprKind::Binary {
            op: BinaryOp::Eq,
            lhs,
            rhs,
        } => {
            if let Some(pred) = dict_eq_pred(lhs, rhs).or_else(|| dict_eq_pred(rhs, lhs)) {
                preds.push(pred);
            }
        }
        _ => {}
    }
}

fn dict_eq_pred(column_side: &Expr, literal_side: &Expr) -> Option<IndexPred> {
    let ExprKind::Column {
        table: None, name, ..
    } = &column_side.kind
    else {
        return None;
    };
    let ExprKind::Literal(Literal::Str(value)) = &literal_side.kind else {
        return None;
    };
    Some(IndexPred::DictEq {
        column: name.clone(),
        value: value.clone(),
    })
}

/// `SINCE`/`UNTIL` lower to one event-time bound each; `LINES`/`BYTES`
/// bounds have no fixed nanosecond edge without consulting the ring
/// (line/byte counts are a segment-selection concern), so they carry no
/// `IndexPred` here — `engine::stream` applies them directly against
/// `Ring`/`LogFile` when it selects segments for `prune.scope`.
fn time_pred(bound: &crate::parser::ast::ScopeBound, now_ns: i64, since: bool) -> Vec<IndexPred> {
    let delta_ns = match bound.unit {
        ScopeUnit::Seconds => i64::try_from(bound.amount.saturating_mul(1_000_000_000)).ok(),
        ScopeUnit::Minutes => i64::try_from(bound.amount.saturating_mul(60 * 1_000_000_000)).ok(),
        ScopeUnit::Hours => i64::try_from(bound.amount.saturating_mul(3_600 * 1_000_000_000)).ok(),
        ScopeUnit::Days => i64::try_from(bound.amount.saturating_mul(86_400 * 1_000_000_000)).ok(),
        ScopeUnit::Lines | ScopeUnit::Bytes => None,
    };
    let Some(delta_ns) = delta_ns else {
        return Vec::new();
    };
    if since {
        vec![IndexPred::TimeRange {
            lo: now_ns.saturating_sub(delta_ns),
            hi: i64::MAX,
        }]
    } else {
        vec![IndexPred::TimeRange {
            lo: i64::MIN,
            hi: now_ns.saturating_sub(delta_ns),
        }]
    }
}

fn scope_now_ns() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos(),
    )
    .unwrap_or(i64::MAX)
}

/// Rewrite every `severity <cmp> '<name>'` (either side) in `WHERE`,
/// `HAVING` and the result list into its numeric code. Moved here from
/// `engine::stream::rewrite` (ADR 0018: this is `codegen::stream`'s once
/// it exists).
pub fn rewrite_severity_literals(select: &mut Select) -> Result<()> {
    if let Some(w) = select.where_clause.as_mut() {
        rewrite_severity_expr(w)?;
    }
    if let Some(h) = select.having.as_mut() {
        rewrite_severity_expr(h)?;
    }
    for c in &mut select.columns {
        if let ResultColumn::Expr { expr: e, .. } = c {
            rewrite_severity_expr(e)?;
        }
    }
    Ok(())
}

fn rewrite_severity_expr(e: &mut Expr) -> Result<()> {
    match &mut e.kind {
        ExprKind::Binary { op, lhs, rhs } => {
            if is_comparison(*op) {
                if is_severity_column(lhs) {
                    rewrite_severity_value(rhs)?;
                } else if is_severity_column(rhs) {
                    rewrite_severity_value(lhs)?;
                }
            }
            rewrite_severity_expr(lhs)?;
            rewrite_severity_expr(rhs)
        }
        ExprKind::Unary { expr: inner, .. } | ExprKind::Paren(inner) => {
            rewrite_severity_expr(inner)
        }
        _ => Ok(()),
    }
}

const fn is_comparison(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
    )
}

fn is_severity_column(e: &Expr) -> bool {
    matches!(&e.kind, ExprKind::Column { name, .. } if name.eq_ignore_ascii_case("severity"))
}

fn rewrite_severity_value(e: &mut Expr) -> Result<()> {
    if let ExprKind::Literal(Literal::Str(s)) = &e.kind {
        let sev = Severity::parse(s).ok_or_else(|| StreamPlanError::InvalidLiteral {
            message: format!(
                "unknown severity '{s}' (expected TRACE, DEBUG, INFO, WARN, ERROR or FATAL, or a number)"
            ),
            span: e.span,
        })?;
        e.kind = ExprKind::Literal(Literal::Integer(i64::from(sev as u8)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn compiled(sql: &str, scope: Scope) -> Result<Program> {
        let select = parse(sql).unwrap();
        compile(&select, scope)
    }

    #[test]
    fn join_is_rejected_with_a_span() {
        let err = compiled(
            "SELECT * FROM log JOIN other ON log.a = other.a",
            Scope::Time(Duration::from_secs(3600)),
        )
        .unwrap_err();
        let StreamPlanError::Rejected { span, .. } = err else {
            panic!("expected Rejected, got {err:?}")
        };
        assert!(span.offset > 0);
    }

    #[test]
    fn order_by_without_limit_rejected_under_scope_all() {
        let err = compiled("SELECT * FROM log ORDER BY timestamp", Scope::All).unwrap_err();
        assert!(matches!(err, StreamPlanError::Rejected { .. }));
    }

    #[test]
    fn order_by_with_limit_is_allowed_under_scope_all() {
        assert!(compiled("SELECT * FROM log ORDER BY timestamp LIMIT 10", Scope::All).is_ok());
    }

    #[test]
    fn aggregate_without_bound_rejected_under_scope_all() {
        let err = compiled("SELECT count(*) FROM log", Scope::All).unwrap_err();
        assert!(matches!(err, StreamPlanError::Rejected { .. }));
    }

    #[test]
    fn aggregate_allowed_under_a_time_scope() {
        assert!(compiled(
            "SELECT count(*) FROM log",
            Scope::Time(Duration::from_secs(3600))
        )
        .is_ok());
    }

    #[test]
    fn dict_eq_predicate_is_lifted_out_of_where() {
        let program = compiled(
            "SELECT * FROM log WHERE facility = 'kern'",
            Scope::Time(Duration::from_secs(3600)),
        )
        .unwrap();
        assert_eq!(
            program.prune.preds,
            vec![IndexPred::DictEq {
                column: "facility".to_string(),
                value: "kern".to_string(),
            }]
        );
    }

    #[test]
    fn mixed_where_keeps_residual_and_lifts_dict_eq() {
        let program = compiled(
            "SELECT * FROM log WHERE facility = 'kern' AND severity >= 13",
            Scope::Time(Duration::from_secs(3600)),
        )
        .unwrap();
        assert_eq!(program.prune.preds.len(), 1);
    }

    #[test]
    fn since_clause_lowers_to_a_time_range_pred() {
        let program = compiled(
            "SELECT * FROM log SINCE 1 h",
            Scope::Time(Duration::from_secs(3600)),
        )
        .unwrap();
        assert!(program
            .prune
            .preds
            .iter()
            .any(|p| matches!(p, IndexPred::TimeRange { .. })));
    }

    #[test]
    fn severity_literal_is_rewritten_before_delegating() {
        let program = compiled(
            "SELECT * FROM log WHERE severity >= 'WARN'",
            Scope::Time(Duration::from_secs(3600)),
        )
        .unwrap();
        assert!(program
            .body
            .columns_to_load()
            .iter()
            .any(|c| c == "severity"));
    }
}
