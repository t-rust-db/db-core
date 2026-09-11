//! Literal rewrites the stream table needs before the batch planner sees
//! the query: `severity >= 'WARN'` becomes `severity >= 13`.
//!
//! The batch VM compares `Str` with `Str` lexicographically and `Str` with
//! `Int` as "no order" (every comparison false), so a text severity can
//! neither order correctly nor match a numeric column. The `severity`
//! column is numeric (OTel code); this pass turns a string literal on the
//! other side of a comparison with it into that code. A name no
//! [`Severity::parse`] knows is a compile error, not a silent empty result.
//!
//! This belongs to the stream planner (`codegen::stream`) once it exists;
//! it lives here so the engine works with the batch planner alone.

use crate::engine::{EngineError, ErrorKind};
use crate::parser::ast::{BinaryOp, Expr, ExprKind, Literal, ResultColumn, Select};
use crate::storage::stream::Severity;

/// Rewrite every `severity <cmp> '<name>'` (either side) in `WHERE`,
/// `HAVING` and the result list into its numeric code.
pub fn severity_literals(select: &mut Select) -> Result<(), EngineError> {
    if let Some(w) = select.where_clause.as_mut() {
        expr(w)?;
    }
    if let Some(h) = select.having.as_mut() {
        expr(h)?;
    }
    for c in &mut select.columns {
        if let ResultColumn::Expr { expr: e, .. } = c {
            expr(e)?;
        }
    }
    Ok(())
}

fn expr(e: &mut Expr) -> Result<(), EngineError> {
    match &mut e.kind {
        ExprKind::Binary { op, lhs, rhs } => {
            if is_comparison(*op) {
                if is_severity_column(lhs) {
                    rewrite_literal(rhs)?;
                } else if is_severity_column(rhs) {
                    rewrite_literal(lhs)?;
                }
            }
            expr(lhs)?;
            expr(rhs)
        }
        ExprKind::Unary { expr: inner, .. } | ExprKind::Paren(inner) => expr(inner),
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

fn rewrite_literal(e: &mut Expr) -> Result<(), EngineError> {
    if let ExprKind::Literal(Literal::Str(s)) = &e.kind {
        let sev = Severity::parse(s).ok_or_else(|| {
            EngineError::new(
                ErrorKind::Compile,
                format!(
                    "unknown severity '{s}' (expected TRACE, DEBUG, INFO, WARN, ERROR or FATAL, or a number)"
                ),
            )
        })?;
        e.kind = ExprKind::Literal(Literal::Integer(i64::from(sev as u8)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn where_of(sql: &str) -> Select {
        let mut s = crate::parser::parse(sql).unwrap();
        severity_literals(&mut s).unwrap();
        s
    }

    fn rhs_literal(e: &Expr) -> Option<&Literal> {
        match &e.kind {
            ExprKind::Binary { rhs, .. } => match &rhs.kind {
                ExprKind::Literal(l) => Some(l),
                _ => None,
            },
            _ => None,
        }
    }

    #[test]
    fn rewrites_name_to_code_case_insensitively() {
        let s = where_of("SELECT * FROM log WHERE severity >= 'warn'");
        assert_eq!(
            rhs_literal(s.where_clause.as_ref().unwrap()),
            Some(&Literal::Integer(13))
        );
    }

    #[test]
    fn rewrites_inside_and_and_parens() {
        let s = where_of("SELECT * FROM log WHERE (severity = 'ERROR') AND facility = 'kern'");
        let w = s.where_clause.unwrap();
        let ExprKind::Binary { lhs, rhs, .. } = &w.kind else {
            panic!()
        };
        let ExprKind::Paren(inner) = &lhs.kind else {
            panic!()
        };
        assert_eq!(rhs_literal(inner), Some(&Literal::Integer(17)));
        assert_eq!(rhs_literal(rhs), Some(&Literal::Str("kern".into())));
    }

    #[test]
    fn unknown_name_is_a_compile_error() {
        let mut s = crate::parser::parse("SELECT * FROM log WHERE severity > 'LOUD'").unwrap();
        let err = severity_literals(&mut s).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Compile);
    }

    #[test]
    fn numbers_and_other_columns_untouched() {
        let s = where_of("SELECT * FROM log WHERE severity >= 13 AND message = 'WARN'");
        let w = s.where_clause.unwrap();
        let ExprKind::Binary { lhs, rhs, .. } = &w.kind else {
            panic!()
        };
        assert_eq!(rhs_literal(lhs), Some(&Literal::Integer(13)));
        assert_eq!(rhs_literal(rhs), Some(&Literal::Str("WARN".into())));
    }
}
