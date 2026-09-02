//! SQL expression and query AST: `Expr`, `Query`, and the pieces they're
//! built from. This crate holds types only — no tokenizing, no parsing
//! (see `sql-parser`), no evaluation (that lives in the executors).
//!
//! `Expr` and `Query` are mutually recursive (`Expr::InSubquery` holds a
//! `Query`, and `Query::where_clause` holds an `Expr`), so both live here
//! rather than splitting `Query` into `sql-parser`.

#![forbid(unsafe_code)]

use sql_types::Literal;

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
    InSubquery { expr: Box<Expr>, subquery: Box<Query> },
}

/// One item in the `SELECT` list: a bare column, an aggregate call like
/// `SUM(amount)` (`*` inside `COUNT(*)` is represented as `None`), or a
/// window function.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    Column(String),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
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
    pub group_by: Vec<String>,
    pub order_by: Option<OrderBy>,
    pub limit: Option<usize>,
}
