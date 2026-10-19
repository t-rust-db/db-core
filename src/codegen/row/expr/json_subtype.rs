// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! sqlite3's JSON subtype, decided from the expression (#651).
//!
//! sqlite3 tags a JSON function's result with a `JSON` subtype that
//! travels with the value, so `json_array(json('[1]'))` nests `[[1]]`
//! while `json_array('[1]')` quotes `["[1]"]`. db-core's `Value` has no
//! subtype: the codegen works out which arguments of a JSON function
//! carry it and names their positions in the `Function` descriptor
//! (`json_array(2);json=0`).
//!
//! The subtype survives what passes a value through unchanged --
//! parentheses, unary `+`, `COLLATE`, `likely`/`unlikely`/`likelihood`,
//! the results of `CASE`/`iif`, `coalesce`/`ifnull`/`nullif`, `min`/`max`
//! and a scalar subquery -- and is lost by anything that computes a new
//! value and by a column, a `FROM` subquery's or a CTE's included. Where
//! it depends on the row:
//!
//! * `json_extract(X, P)` gives an object or array the subtype and any
//!   other value none. It becomes [`JSON_EXTRACT_SUBTYPED`], which
//!   returns the JSON text either way.
//! * A plain result next to a JSON one -- a `CASE` branch, a `coalesce`
//!   argument -- becomes [`JSON_QUOTE_NULLABLE`]: `json_quote`, which is
//!   how it would be embedded anyway.
//!
//! Both keep NULL as NULL, so `coalesce` still picks the same argument,
//! but change the value itself: where something still compares it
//! (`nullif`'s first argument, `min`/`max`, a subquery's result) a mix
//! is refused rather than guessed.

use crate::codegen::row::CodegenError;
use crate::functions::{JSON_EXTRACT_SUBTYPED, JSON_QUOTE_NULLABLE};
use crate::parser::ast::{Expr, ExprKind, FunctionArgs, Literal, ResultColumn, UnaryOp};
use crate::parser::Span;

/// The functions whose result carries the JSON subtype.
const JSON_RESULTS: &[&str] = &[
    "json",
    "json_array",
    "json_insert",
    "json_object",
    "json_patch",
    "json_quote",
    "json_remove",
    "json_replace",
    "json_set",
];

/// What an expression's value is, as a JSON function's argument.
enum Arg {
    /// Never carries the JSON subtype.
    Plain,
    /// Always NULL, which embeds as `null` either way.
    Null,
    /// Carries the JSON subtype: the expression, rewritten where needed.
    Json(Expr),
}

/// Whether argument `i` of `name(arity args)` is a value a JSON function
/// embeds, where the JSON subtype matters.
fn is_value_position(name: &str, i: usize) -> bool {
    match name {
        "json_array" => true,
        "json_object" => !i.is_multiple_of(2),
        "json_quote" => i == 0,
        "json_insert" | "json_replace" | "json_set" => i >= 2 && i.is_multiple_of(2),
        _ => false,
    }
}

/// The positions of the arguments of a call to `name` (lowercase) that
/// carry the JSON subtype, and the arguments to compile instead of `args`
/// if any had to be rewritten.
pub(crate) fn mark(
    name: &str,
    args: &[Expr],
) -> Result<(Vec<usize>, Option<Vec<Expr>>), CodegenError> {
    let mut rewritten: Option<Vec<Expr>> = None;
    let mut json = Vec::new();
    for (i, arg) in args.iter().enumerate() {
        if !is_value_position(name, i) {
            continue;
        }
        if let Arg::Json(expr) = classify(arg, true)? {
            json.push(i);
            if expr != *arg {
                let out = rewritten.get_or_insert_with(|| args.to_vec());
                if let Some(slot) = out.get_mut(i) {
                    *slot = expr;
                }
            }
        }
    }
    Ok((json, rewritten))
}

/// Whether `expr` always carries the JSON subtype, needing no rewrite:
/// an aggregate's argument, whose result keeps it through `min`/`max`.
pub(crate) fn carries_json_subtype(expr: &Expr) -> bool {
    matches!(classify(expr, false), Ok(Arg::Json(_)))
}

/// `expr` as a JSON function's argument, rewritten if `rewrite` allows.
fn classify(expr: &Expr, rewrite: bool) -> Result<Arg, CodegenError> {
    match &expr.kind {
        ExprKind::Literal(Literal::Null) => Ok(Arg::Null),
        ExprKind::Paren(inner) => {
            classify(inner, rewrite).map(|a| rewrap(a, expr, |e| ExprKind::Paren(Box::new(e))))
        }
        ExprKind::Unary {
            op: UnaryOp::Plus,
            expr: inner,
        } => classify(inner, rewrite).map(|a| {
            rewrap(a, expr, |e| ExprKind::Unary {
                op: UnaryOp::Plus,
                expr: Box::new(e),
            })
        }),
        ExprKind::Collate {
            expr: inner,
            collation,
        } => classify(inner, rewrite).map(|a| {
            rewrap(a, expr, |e| ExprKind::Collate {
                expr: Box::new(e),
                collation: collation.clone(),
            })
        }),
        ExprKind::Case {
            operand,
            whens,
            else_,
        } => {
            let mut results: Vec<&Expr> = whens.iter().map(|(_, then)| then).collect();
            results.extend(else_.as_deref());
            let mut results = match branches(&results, &vec![rewrite; results.len()])? {
                Branches::Json(results) => results,
                Branches::Not(arg) => return Ok(arg),
            };
            let else_ = else_
                .as_ref()
                .map(|_| Box::new(results.pop().unwrap_or_else(null)));
            let thens = results;
            let whens = whens
                .iter()
                .zip(thens)
                .map(|((when, _), then)| (when.clone(), then))
                .collect();
            Ok(Arg::Json(Expr {
                kind: ExprKind::Case {
                    operand: operand.clone(),
                    whens,
                    else_,
                },
                span: expr.span,
            }))
        }
        ExprKind::Subquery(select) => {
            let [ResultColumn::Expr { expr: result, .. }] = select.columns.as_slice() else {
                return Ok(Arg::Plain);
            };
            if !select.compound.is_empty() {
                return Ok(Arg::Plain);
            }
            Ok(match classify(result, false)? {
                Arg::Json(_) => Arg::Json(expr.clone()),
                other => other,
            })
        }
        ExprKind::FunctionCall {
            name,
            distinct: false,
            args: FunctionArgs::List(args),
            tail: None,
        } => call(expr, &name.to_ascii_lowercase(), args, rewrite),
        _ => Ok(Arg::Plain),
    }
}

/// [`classify`] for a function call `expr`, `name(args)`.
fn call(expr: &Expr, name: &str, args: &[Expr], rewrite: bool) -> Result<Arg, CodegenError> {
    if JSON_RESULTS.contains(&name) {
        return Ok(Arg::Json(expr.clone()));
    }
    let n = args.len();
    match (name, args) {
        ("json_extract", [_, _]) => {
            if !rewrite {
                return Err(mixed());
            }
            Ok(Arg::Json(with_args(
                expr,
                JSON_EXTRACT_SUBTYPED,
                args.to_vec(),
            )))
        }
        // With several paths, always a JSON array.
        ("json_extract", _) if n >= 3 => Ok(Arg::Json(expr.clone())),
        ("likely" | "unlikely", [inner]) | ("likelihood", [inner, _]) => {
            let rest = args.get(1..).unwrap_or(&[]);
            classify(inner, rewrite).map(|a| {
                rewrap(a, expr, |e| {
                    let mut args = vec![e];
                    args.extend_from_slice(rest);
                    call_kind(name, args)
                })
            })
        }
        ("iif" | "if", _) if n >= 2 => classify(&super::value::iif_as_case(expr, args), rewrite),
        ("coalesce" | "ifnull", _) if n >= 2 => {
            let refs: Vec<&Expr> = args.iter().collect();
            match branches(&refs, &vec![rewrite; n])? {
                Branches::Json(results) => Ok(Arg::Json(with_args(expr, name, results))),
                Branches::Not(arg) => Ok(arg),
            }
        }
        ("nullif", [first, _]) => Ok(match classify(first, false)? {
            Arg::Json(_) => Arg::Json(expr.clone()),
            other => other,
        }),
        ("min" | "max", _) if n >= 1 => {
            let refs: Vec<&Expr> = args.iter().collect();
            Ok(match branches(&refs, &vec![false; n])? {
                Branches::Json(_) => Arg::Json(expr.clone()),
                Branches::Not(arg) => arg,
            })
        }
        _ => Ok(Arg::Plain),
    }
}

/// The value of an expression that yields one of several results.
enum Branches {
    /// None carries the JSON subtype: [`Arg::Plain`] or [`Arg::Null`].
    Not(Arg),
    /// Some do: every result, rewritten so that all of them do.
    Json(Vec<Expr>),
}

/// [`Branches`] for `results`, the `i`th rewritten if `rewrite[i]`
/// allows: when any is JSON, every plain one is quoted.
fn branches(results: &[&Expr], rewrite: &[bool]) -> Result<Branches, CodegenError> {
    let mut args = Vec::with_capacity(results.len());
    for (&result, &rewrite) in results.iter().zip(rewrite) {
        args.push((result, rewrite, classify(result, rewrite)?));
    }
    if !args.iter().any(|(_, _, a)| matches!(a, Arg::Json(_))) {
        let all_null = args.iter().all(|(_, _, a)| matches!(a, Arg::Null));
        return Ok(Branches::Not(if all_null { Arg::Null } else { Arg::Plain }));
    }
    let mut out = Vec::with_capacity(args.len());
    for (result, rewrite, arg) in args {
        out.push(match arg {
            Arg::Json(e) => e,
            Arg::Null => result.clone(),
            Arg::Plain if rewrite => with_args(result, JSON_QUOTE_NULLABLE, vec![result.clone()]),
            Arg::Plain => return Err(mixed()),
        });
    }
    Ok(Branches::Json(out))
}

/// A NULL literal.
fn null() -> Expr {
    Expr {
        kind: ExprKind::Literal(Literal::Null),
        span: Span::default(),
    }
}

/// `arg` with a JSON inner expression put back into `outer`'s shape.
fn rewrap(arg: Arg, outer: &Expr, kind: impl FnOnce(Expr) -> ExprKind) -> Arg {
    match arg {
        Arg::Json(inner) => Arg::Json(Expr {
            kind: kind(inner),
            span: outer.span,
        }),
        other => other,
    }
}

/// The call `name(args)` at `at`'s span.
fn with_args(at: &Expr, name: &str, args: Vec<Expr>) -> Expr {
    Expr {
        kind: call_kind(name, args),
        span: at.span,
    }
}

/// A plain call `name(args)`.
fn call_kind(name: &str, args: Vec<Expr>) -> ExprKind {
    ExprKind::FunctionCall {
        name: name.to_string(),
        distinct: false,
        args: FunctionArgs::List(args),
        tail: None,
    }
}

/// The refusal for an argument that carries the JSON subtype on some
/// rows only, where it cannot be rewritten.
fn mixed() -> CodegenError {
    CodegenError::Unsupported {
        reason: "a JSON function argument that carries the JSON subtype on some rows only"
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::row::{parse_select, ParseOutcome};

    /// `mark` for the single call in `SELECT <call>`: the marked positions
    /// and the arguments compiled in its place, printed back as SQL names.
    fn marks(call: &str) -> Result<(Vec<usize>, Vec<String>), CodegenError> {
        let ParseOutcome::Accepted(select) = parse_select(&format!("SELECT {call}")) else {
            panic!("{call} does not parse");
        };
        let [ResultColumn::Expr { expr, .. }] = select.columns.as_slice() else {
            panic!("{call}: one result column");
        };
        let ExprKind::FunctionCall {
            name,
            args: FunctionArgs::List(args),
            ..
        } = &expr.kind
        else {
            panic!("{call} is not a call");
        };
        let (json, rewritten) = mark(&name.to_ascii_lowercase(), args)?;
        let called = rewritten
            .unwrap_or_else(|| args.clone())
            .iter()
            .map(|a| match &a.kind {
                ExprKind::FunctionCall { name, .. } => name.clone(),
                ExprKind::Case { .. } => "CASE".to_string(),
                _ => "-".to_string(),
            })
            .collect();
        Ok((json, called))
    }

    #[test]
    fn json_results_are_marked_in_value_positions_only() {
        let (json, _) = marks("json_array(json('[1]'), '[1]', json_object())").unwrap();
        assert_eq!(json, vec![0, 2]);
        let (json, _) = marks("json_object(json('\"k\"'), json('[1]'))").unwrap();
        assert_eq!(json, vec![1]);
        let (json, _) = marks("json_set('{}', json('\"$\"'), json('[1]'), '$.b', 1)").unwrap();
        assert_eq!(json, vec![2]);
        let (json, _) = marks("json_type(json('[1]'))").unwrap();
        assert!(json.is_empty());
        let (json, _) = marks("json_quote(json_extract('[1]', '$', '$'))").unwrap();
        assert_eq!(json, vec![0]);
    }

    #[test]
    fn the_subtype_passes_through_value_preserving_expressions() {
        for arg in [
            "(json('[1]'))",
            "+json('[1]')",
            "json('[1]') COLLATE nocase",
            "likely(json('[1]'))",
            "likelihood(json('[1]'), 0.5)",
            "nullif(json('[1]'), 1)",
            "min(json('[1]'), json('[2]'))",
            "(SELECT json('[1]'))",
            "coalesce(NULL, json('[1]'))",
            "CASE WHEN 1 THEN json('[1]') END",
        ] {
            let (json, _) = marks(&format!("json_array({arg})")).unwrap();
            assert_eq!(json, vec![0], "{arg}");
        }
        for arg in [
            "upper(json('[1]'))",
            "json('[1]') || ''",
            "(SELECT '[1]')",
            "(SELECT json('[1]') UNION ALL SELECT 1)",
            "CASE WHEN 1 THEN NULL END",
            "count(DISTINCT json('[1]'))",
            "nullif('[1]', 1)",
        ] {
            let (json, called) = marks(&format!("json_array({arg})")).unwrap();
            assert!(json.is_empty(), "{arg}");
            assert_ne!(called, vec![JSON_QUOTE_NULLABLE.to_string()], "{arg}");
        }
    }

    #[test]
    fn row_dependent_subtypes_are_rewritten() {
        let (json, called) = marks("json_array(json_extract('[1]', '$'))").unwrap();
        assert_eq!(
            (json, called),
            (vec![0], vec![JSON_EXTRACT_SUBTYPED.to_string()])
        );
        let (json, called) = marks("json_array(iif(1, json('[1]'), 'x'))").unwrap();
        assert_eq!((json, called), (vec![0], vec!["CASE".to_string()]));
        let (json, called) = marks("json_array(coalesce('x', json('[1]')))").unwrap();
        assert_eq!((json, called), (vec![0], vec!["coalesce".to_string()]));
        let (json, called) =
            marks("json_array(CASE WHEN 1 THEN json('[1]') WHEN 2 THEN NULL ELSE 'x' END)")
                .unwrap();
        assert_eq!((json, called), (vec![0], vec!["CASE".to_string()]));
        let (json, called) = marks("json_array(likely(json_extract('[1]', '$')))").unwrap();
        assert_eq!((json, called), (vec![0], vec!["likely".to_string()]));
    }

    #[test]
    fn a_mix_that_is_compared_is_refused() {
        for arg in [
            "nullif(json_extract('[1]', '$'), '')",
            "max(json('[1]'), '[0]')",
            "(SELECT json_extract('[1]', '$'))",
            "(SELECT CASE WHEN 1 THEN json('[1]') ELSE 'x' END)",
        ] {
            let refused = marks(&format!("json_array({arg})"));
            assert!(
                matches!(refused, Err(CodegenError::Unsupported { .. })),
                "{arg}: {refused:?}"
            );
        }
    }
}
