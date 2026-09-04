//! SQL expression and query AST: `Expr`, `Query`, and the pieces they're
//! built from. This crate holds types only — no tokenizing, no parsing
//! (see `sql-parser`), no evaluation (that lives in the executors).
//!
//! `Expr` and `Query` are mutually recursive (`Expr::InSubquery` holds a
//! `Query`, and `Query::where_clause` holds an `Expr`), so both live here
//! rather than splitting `Query` into `sql-parser`.

#![forbid(unsafe_code)]

use crate::types::Literal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    /// `||` string concatenation (DuckDB/Postgres-style; implicitly
    /// stringifies non-`Str` operands rather than erroring).
    Concat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl AggFunc {
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "COUNT" => Some(AggFunc::Count),
            "SUM" => Some(AggFunc::Sum),
            "AVG" => Some(AggFunc::Avg),
            "MIN" => Some(AggFunc::Min),
            "MAX" => Some(AggFunc::Max),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Column(String),
    Literal(Literal),
    BinaryOp(Box<Expr>, BinOp, Box<Expr>),
    /// `expr IN (SELECT ...)` -- a semi-join: `expr` is compared against
    /// every row the subquery's (single-column) `SELECT` list produces.
    InSubquery {
        expr: Box<Expr>,
        subquery: Box<Query>,
    },
    /// `NOT expr`.
    Not(Box<Expr>),
    /// Unary `-expr`. Unary `+expr` is a no-op and isn't represented --
    /// the parser consumes and discards a leading `+`.
    Neg(Box<Expr>),
    /// `expr IS [NOT] NULL`.
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
}

/// One item in the `SELECT` list: a bare column, `*` (all columns -- see
/// `SelectItem::Star` docs), an aggregate call like `SUM(amount)` (`*`
/// inside `COUNT(*)` is represented as `None`), or a window function.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    Column(String),
    /// Bare `*` in the `SELECT` list. `sql-parser` has no access to a
    /// table's schema (it never reads Parquet footers -- that's
    /// `column-rs`'s job), so this can't be expanded to a concrete column
    /// list at parse time. It's carried through as this AST-level marker
    /// and left for the caller/executor -- which does have the schema --
    /// to expand when it compiles the query.
    Star,
    Agg(AggFunc, Option<String>),
    Window(WindowSpec),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowFunc {
    RowNumber,
    Rank,
    DenseRank,
    Lag,
    Lead,
    FirstValue,
    LastValue,
    Sum,
    Avg,
    Count,
}

/// `func(...) OVER (PARTITION BY ... ORDER BY ...)`. `offset` is only used
/// by `LAG`/`LEAD` (default 1 when omitted).
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSpec {
    pub func: WindowFunc,
    pub arg: Option<String>,
    pub offset: Option<i64>,
    pub partition_by: Vec<String>,
    pub order_by: Vec<(String, bool)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderBy {
    pub column: String,
    pub descending: bool,
}

/// Converging toward sqlite-rs's 5-variant `JoinOp`
/// (`~/wc/sqlite-rs/src/parser/ast.rs`) rather than diverging from it --
/// `NATURAL`/`USING` aren't represented here since `Join` below carries
/// only an `ON`-style equi-join condition, unlike sqlite-rs's
/// `JoinConstraint`. Table aliases (`sql-parser`'s `FROM orders o`) are a
/// parse-time-only rewrite of qualified column names back to real table
/// names -- they never reach this AST.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    /// `RIGHT [OUTER] JOIN`.
    Right,
    /// `FULL [OUTER] JOIN`.
    Full,
    /// `CROSS JOIN` -- an unconditional cross product. The parser requires
    /// a `LIMIT` to be present alongside a `CROSS JOIN` (DO-178C bounded-
    /// execution principle: an unbounded cross product over two large
    /// tables has no natural row cap the way an equi-join's selectivity
    /// gives it).
    Cross,
}

/// An equi-join against another table: `[INNER|LEFT] JOIN table ON
/// left_col = right_col` (column names may be qualified, e.g. `t.col`).
#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    pub kind: JoinKind,
    pub table: String,
    pub left_col: String,
    pub right_col: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub columns: Vec<SelectItem>,
    pub from: String,
    pub joins: Vec<Join>,
    pub where_clause: Option<Expr>,
    pub distinct: bool,
    pub group_by: Vec<String>,
    pub order_by: Option<OrderBy>,
    pub limit: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_query(from: &str) -> Query {
        Query {
            columns: vec![SelectItem::Column("x".into())],
            from: from.into(),
            joins: vec![],
            where_clause: None,
            distinct: false,
            group_by: vec![],
            order_by: None,
            limit: None,
        }
    }

    #[test]
    fn agg_func_from_name_valid_case_insensitive() {
        for (name, expected) in [
            ("count", AggFunc::Count),
            ("COUNT", AggFunc::Count),
            ("Count", AggFunc::Count),
            ("sum", AggFunc::Sum),
            ("SUM", AggFunc::Sum),
            ("avg", AggFunc::Avg),
            ("AVG", AggFunc::Avg),
            ("min", AggFunc::Min),
            ("MIN", AggFunc::Min),
            ("max", AggFunc::Max),
            ("MAX", AggFunc::Max),
        ] {
            assert_eq!(AggFunc::from_name(name), Some(expected), "name = {name}");
        }
    }

    #[test]
    fn agg_func_from_name_invalid() {
        assert_eq!(AggFunc::from_name("bogus"), None);
        assert_eq!(AggFunc::from_name(""), None);
        assert_eq!(AggFunc::from_name("counter"), None);
    }

    #[test]
    fn binop_and_join_kind_equality() {
        assert_eq!(BinOp::Add, BinOp::Add);
        assert_ne!(BinOp::Add, BinOp::Sub);
        assert_eq!(JoinKind::Inner, JoinKind::Inner);
        assert_ne!(JoinKind::Inner, JoinKind::Left);
    }

    #[test]
    fn expr_binary_op_roundtrip() {
        let expr = Expr::BinaryOp(
            Box::new(Expr::Column("a".into())),
            BinOp::Gt,
            Box::new(Expr::Literal(Literal::Int(5))),
        );
        match &expr {
            Expr::BinaryOp(lhs, op, rhs) => {
                assert_eq!(**lhs, Expr::Column("a".into()));
                assert_eq!(*op, BinOp::Gt);
                assert_eq!(**rhs, Expr::Literal(Literal::Int(5)));
            }
            _ => panic!("expected BinaryOp"),
        }
        assert_eq!(expr.clone(), expr);
    }

    #[test]
    fn select_item_variants_roundtrip() {
        let col = SelectItem::Column("name".into());
        assert_eq!(col.clone(), col);

        let agg_star = SelectItem::Agg(AggFunc::Count, None);
        let agg_named = SelectItem::Agg(AggFunc::Sum, Some("amount".into()));
        assert_ne!(agg_star, agg_named);

        let window = SelectItem::Window(WindowSpec {
            func: WindowFunc::RowNumber,
            arg: None,
            offset: None,
            partition_by: vec!["dept".into()],
            order_by: vec![("salary".into(), true)],
        });
        assert_eq!(window.clone(), window);
    }

    #[test]
    fn window_spec_fields_roundtrip() {
        let spec = WindowSpec {
            func: WindowFunc::Lag,
            arg: Some("amount".into()),
            offset: Some(2),
            partition_by: vec!["region".into(), "year".into()],
            order_by: vec![("date".into(), false)],
        };
        assert_eq!(spec.func, WindowFunc::Lag);
        assert_eq!(spec.arg, Some("amount".into()));
        assert_eq!(spec.offset, Some(2));
        assert_eq!(
            spec.partition_by,
            vec!["region".to_string(), "year".to_string()]
        );
        assert_eq!(spec.order_by, vec![("date".to_string(), false)]);
        assert_eq!(spec.clone(), spec);
    }

    #[test]
    fn order_by_and_join_roundtrip() {
        let ob = OrderBy {
            column: "id".into(),
            descending: true,
        };
        assert_eq!(ob.column, "id");
        assert!(ob.descending);
        assert_eq!(ob.clone(), ob);

        let join = Join {
            kind: JoinKind::Left,
            table: "orders".into(),
            left_col: "customers.id".into(),
            right_col: "orders.customer_id".into(),
        };
        assert_eq!(join.kind, JoinKind::Left);
        assert_eq!(join.table, "orders");
        assert_eq!(join.left_col, "customers.id");
        assert_eq!(join.right_col, "orders.customer_id");
        assert_eq!(join.clone(), join);
    }

    #[test]
    fn query_fields_roundtrip() {
        let q = Query {
            columns: vec![
                SelectItem::Column("id".into()),
                SelectItem::Agg(AggFunc::Count, None),
            ],
            from: "customers".into(),
            joins: vec![Join {
                kind: JoinKind::Inner,
                table: "orders".into(),
                left_col: "customers.id".into(),
                right_col: "orders.customer_id".into(),
            }],
            distinct: false,
            where_clause: Some(Expr::BinaryOp(
                Box::new(Expr::Column("age".into())),
                BinOp::Ge,
                Box::new(Expr::Literal(Literal::Int(18))),
            )),
            group_by: vec!["id".into()],
            order_by: Some(OrderBy {
                column: "id".into(),
                descending: false,
            }),
            limit: Some(10),
        };

        assert_eq!(q.from, "customers");
        assert_eq!(q.columns.len(), 2);
        assert_eq!(q.joins.len(), 1);
        assert!(q.where_clause.is_some());
        assert_eq!(q.group_by, vec!["id".to_string()]);
        assert_eq!(q.limit, Some(10));
        assert_eq!(q.clone(), q);
    }

    #[test]
    fn expr_in_subquery_construction() {
        let inner = empty_query("orders");
        let expr = Expr::InSubquery {
            expr: Box::new(Expr::Column("customer_id".into())),
            subquery: Box::new(inner.clone()),
        };

        match &expr {
            Expr::InSubquery {
                expr: inner_expr,
                subquery,
            } => {
                assert_eq!(**inner_expr, Expr::Column("customer_id".into()));
                assert_eq!(**subquery, inner);
            }
            _ => panic!("expected InSubquery"),
        }
        assert_eq!(expr.clone(), expr);
    }

    #[test]
    fn expr_in_subquery_nested_two_levels() {
        let innermost = empty_query("leaf");
        let mid = Query {
            where_clause: Some(Expr::InSubquery {
                expr: Box::new(Expr::Column("id".into())),
                subquery: Box::new(innermost.clone()),
            }),
            ..empty_query("mid")
        };
        let outer = Expr::InSubquery {
            expr: Box::new(Expr::Column("id".into())),
            subquery: Box::new(mid.clone()),
        };

        if let Expr::InSubquery { subquery, .. } = &outer {
            assert_eq!(subquery.where_clause, mid.where_clause);
            if let Some(Expr::InSubquery {
                subquery: nested, ..
            }) = &subquery.where_clause
            {
                assert_eq!(**nested, innermost);
            } else {
                panic!("expected nested InSubquery in where_clause");
            }
        } else {
            panic!("expected InSubquery");
        }
    }

    #[test]
    fn literal_variants_in_expr() {
        assert_eq!(
            Expr::Literal(Literal::Int(1)),
            Expr::Literal(Literal::Int(1))
        );
        assert_ne!(
            Expr::Literal(Literal::Int(1)),
            Expr::Literal(Literal::Float(1.0))
        );
        assert_eq!(
            Expr::Literal(Literal::Str("hi".into())),
            Expr::Literal(Literal::Str("hi".into()))
        );
        assert_eq!(
            Expr::Literal(Literal::Str("".into())),
            Expr::Literal(Literal::Str(String::new()))
        );
    }
}
