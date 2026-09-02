//! Minimal SQL parser (#27, #63, #65, #67-70): `SELECT ... FROM ...
//! [[INNER|LEFT] JOIN table ON col = col ...] [WHERE ... | WHERE col IN
//! (SELECT ...)] [GROUP BY ...] [ORDER BY ...] [LIMIT ...]`, restricted to
//! the analytics subset the query VM executes. Joins are equi-joins only (no
//! table aliases; qualify columns with the real table name, e.g.
//! `orders.id`). The only subquery form is `col IN (SELECT ...)` (a
//! semi-join) as the *entire* `WHERE` clause -- it can't be combined with
//! other conditions via `AND`/`OR`. `SELECT` items may also be a window
//! function (`ROW_NUMBER`/`RANK`/`DENSE_RANK`/`LAG`/`LEAD`/`FIRST_VALUE`/
//! `LAST_VALUE`/`SUM`/`AVG`/`COUNT`) with `OVER (PARTITION BY ... ORDER BY
//! ...)`.
//!
//! Produces `sql_expr::Query` -- the AST types themselves live in
//! `sql-expr`, not here.

#![forbid(unsafe_code)]

use std::fmt;

use sql_expr::{
    AggFunc, BinOp, Expr, Join, JoinKind, OrderBy, Query, SelectItem, WindowFunc, WindowSpec,
};
use sql_types::Literal;

#[derive(Debug, PartialEq)]
pub enum ParseError {
    UnexpectedEof,
    Unexpected(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::UnexpectedEof => write!(f, "unexpected end of query"),
            ParseError::Unexpected(tok) => write!(f, "unexpected token: {tok}"),
        }
    }
}

impl std::error::Error for ParseError {}

pub type Result<T> = std::result::Result<T, ParseError>;

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    Int(i64),
    Float(f64),
    Str(String),
    Star,
    Comma,
    Dot,
    LParen,
    RParen,
    Op(String),
}

fn tokenize(input: &str) -> Result<Vec<Token>> {
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    let mut tokens = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '*' => {
                tokens.push(Token::Star);
                i += 1;
            }
            ',' => {
                tokens.push(Token::Comma);
                i += 1;
            }
            '.' => {
                tokens.push(Token::Dot);
                i += 1;
            }
            '(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            ')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            '\'' => {
                let start = i + 1;
                let mut j = start;
                while j < chars.len() && chars[j] != '\'' {
                    j += 1;
                }
                if j >= chars.len() {
                    return Err(ParseError::Unexpected(
                        "unterminated string literal".to_string(),
                    ));
                }
                tokens.push(Token::Str(chars[start..j].iter().collect()));
                i = j + 1;
            }
            '=' | '<' | '>' | '!' | '+' | '-' | '/' => {
                let mut op = String::from(c);
                if i + 1 < chars.len() && chars[i + 1] == '=' && matches!(c, '<' | '>' | '!' | '=')
                {
                    op.push('=');
                    i += 2;
                } else {
                    i += 1;
                }
                tokens.push(Token::Op(op));
            }
            _ if c.is_ascii_digit() => {
                let start = i;
                let mut is_float = false;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    if chars[i] == '.' {
                        is_float = true;
                    }
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                if is_float {
                    tokens.push(Token::Float(text.parse().unwrap()));
                } else {
                    tokens.push(Token::Int(text.parse().unwrap()));
                }
            }
            _ if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                tokens.push(Token::Ident(chars[start..i].iter().collect()));
            }
            other => {
                return Err(ParseError::Unexpected(format!(
                    "unexpected character '{other}'"
                )))
            }
        }
    }
    Ok(tokens)
}

/// `(partition_by_columns, order_by_columns_with_direction)`.
type OverClause = (Vec<String>, Vec<(String, bool)>);

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Result<Token> {
        let tok = self
            .tokens
            .get(self.pos)
            .cloned()
            .ok_or(ParseError::UnexpectedEof)?;
        self.pos += 1;
        Ok(tok)
    }

    fn expect_keyword(&mut self, keyword: &str) -> Result<()> {
        match self.next()? {
            Token::Ident(word) if word.eq_ignore_ascii_case(keyword) => Ok(()),
            other => Err(ParseError::Unexpected(format!(
                "{other:?}, expected {keyword}"
            ))),
        }
    }

    fn peek_keyword(&self, keyword: &str) -> bool {
        matches!(self.peek(), Some(Token::Ident(word)) if word.eq_ignore_ascii_case(keyword))
    }

    /// A (possibly qualified) identifier: `col` or `table.col`.
    fn ident(&mut self) -> Result<String> {
        let mut name = match self.next()? {
            Token::Ident(name) => name,
            other => {
                return Err(ParseError::Unexpected(format!(
                    "{other:?}, expected identifier"
                )))
            }
        };
        while matches!(self.peek(), Some(Token::Dot)) {
            self.next()?;
            match self.next()? {
                Token::Ident(part) => {
                    name.push('.');
                    name.push_str(&part);
                }
                other => {
                    return Err(ParseError::Unexpected(format!(
                        "{other:?}, expected identifier after '.'"
                    )))
                }
            }
        }
        Ok(name)
    }

    fn parse_query(&mut self) -> Result<Query> {
        self.expect_keyword("SELECT")?;
        let columns = self.parse_select_list()?;
        self.expect_keyword("FROM")?;
        let from = self.ident()?;

        let mut joins = Vec::new();
        loop {
            let kind = if self.peek_keyword("JOIN") {
                self.next()?;
                JoinKind::Inner
            } else if self.peek_keyword("INNER") {
                self.next()?;
                self.expect_keyword("JOIN")?;
                JoinKind::Inner
            } else if self.peek_keyword("LEFT") {
                self.next()?;
                if self.peek_keyword("OUTER") {
                    self.next()?;
                }
                self.expect_keyword("JOIN")?;
                JoinKind::Left
            } else {
                break;
            };
            let table = self.ident()?;
            self.expect_keyword("ON")?;
            let left_col = self.ident()?;
            match self.next()? {
                Token::Op(op) if op == "=" => {}
                other => {
                    return Err(ParseError::Unexpected(format!(
                        "{other:?}, expected = in ON clause"
                    )))
                }
            }
            let right_col = self.ident()?;
            joins.push(Join {
                kind,
                table,
                left_col,
                right_col,
            });
        }

        let where_clause = if self.peek_keyword("WHERE") {
            self.next()?;
            Some(self.parse_expr()?)
        } else {
            None
        };

        let group_by = if self.peek_keyword("GROUP") {
            self.next()?;
            self.expect_keyword("BY")?;
            self.parse_ident_list()?
        } else {
            Vec::new()
        };

        let order_by = if self.peek_keyword("ORDER") {
            self.next()?;
            self.expect_keyword("BY")?;
            let column = self.ident()?;
            let descending = if self.peek_keyword("DESC") {
                self.next()?;
                true
            } else if self.peek_keyword("ASC") {
                self.next()?;
                false
            } else {
                false
            };
            Some(OrderBy { column, descending })
        } else {
            None
        };

        let limit = if self.peek_keyword("LIMIT") {
            self.next()?;
            match self.next()? {
                Token::Int(n) if n >= 0 => Some(n as usize),
                other => {
                    return Err(ParseError::Unexpected(format!(
                        "{other:?}, expected LIMIT count"
                    )))
                }
            }
        } else {
            None
        };

        Ok(Query {
            columns,
            from,
            joins,
            where_clause,
            group_by,
            order_by,
            limit,
        })
    }

    /// Top-level entry point: parses a full query and rejects trailing
    /// tokens. `parse_query` itself is also used recursively for subqueries
    /// (e.g. `IN (SELECT ...)`), which must stop at the subquery's closing
    /// `)` rather than expecting end-of-input.
    fn parse_top_level_query(&mut self) -> Result<Query> {
        let query = self.parse_query()?;
        if self.pos != self.tokens.len() {
            return Err(ParseError::Unexpected(format!(
                "trailing tokens: {:?}",
                &self.tokens[self.pos..]
            )));
        }
        Ok(query)
    }

    fn parse_ident_list(&mut self) -> Result<Vec<String>> {
        let mut idents = vec![self.ident()?];
        while matches!(self.peek(), Some(Token::Comma)) {
            self.next()?;
            idents.push(self.ident()?);
        }
        Ok(idents)
    }

    fn parse_select_list(&mut self) -> Result<Vec<SelectItem>> {
        let mut items = vec![self.parse_select_item()?];
        while matches!(self.peek(), Some(Token::Comma)) {
            self.next()?;
            items.push(self.parse_select_item()?);
        }
        Ok(items)
    }

    fn parse_select_item(&mut self) -> Result<SelectItem> {
        let name = self.ident()?;
        if !matches!(self.peek(), Some(Token::LParen)) {
            return Ok(SelectItem::Column(name));
        }
        self.next()?; // consume '('

        let upper = name.to_ascii_uppercase();
        match upper.as_str() {
            "ROW_NUMBER" | "RANK" | "DENSE_RANK" => {
                self.expect_rparen()?;
                self.expect_keyword("OVER")?;
                let (partition_by, order_by) = self.parse_over_clause()?;
                let func = match upper.as_str() {
                    "ROW_NUMBER" => WindowFunc::RowNumber,
                    "RANK" => WindowFunc::Rank,
                    _ => WindowFunc::DenseRank,
                };
                Ok(SelectItem::Window(WindowSpec {
                    func,
                    arg: None,
                    offset: None,
                    partition_by,
                    order_by,
                }))
            }
            "LAG" | "LEAD" => {
                let arg = self.ident()?;
                let offset = if matches!(self.peek(), Some(Token::Comma)) {
                    self.next()?;
                    match self.next()? {
                        Token::Int(n) => Some(n),
                        other => {
                            return Err(ParseError::Unexpected(format!(
                                "{other:?}, expected integer offset"
                            )))
                        }
                    }
                } else {
                    None
                };
                self.expect_rparen()?;
                self.expect_keyword("OVER")?;
                let (partition_by, order_by) = self.parse_over_clause()?;
                let func = if upper == "LAG" {
                    WindowFunc::Lag
                } else {
                    WindowFunc::Lead
                };
                Ok(SelectItem::Window(WindowSpec {
                    func,
                    arg: Some(arg),
                    offset,
                    partition_by,
                    order_by,
                }))
            }
            "FIRST_VALUE" | "LAST_VALUE" => {
                let arg = self.ident()?;
                self.expect_rparen()?;
                self.expect_keyword("OVER")?;
                let (partition_by, order_by) = self.parse_over_clause()?;
                let func = if upper == "FIRST_VALUE" {
                    WindowFunc::FirstValue
                } else {
                    WindowFunc::LastValue
                };
                Ok(SelectItem::Window(WindowSpec {
                    func,
                    arg: Some(arg),
                    offset: None,
                    partition_by,
                    order_by,
                }))
            }
            _ => {
                let arg = if matches!(self.peek(), Some(Token::Star)) {
                    self.next()?;
                    None
                } else {
                    Some(self.ident()?)
                };
                self.expect_rparen()?;
                if self.peek_keyword("OVER") {
                    self.next()?;
                    let (partition_by, order_by) = self.parse_over_clause()?;
                    let func = match upper.as_str() {
                        "SUM" => WindowFunc::Sum,
                        "AVG" => WindowFunc::Avg,
                        "COUNT" => WindowFunc::Count,
                        _ => {
                            return Err(ParseError::Unexpected(format!(
                                "{name} OVER (...) is not a supported window function"
                            )))
                        }
                    };
                    Ok(SelectItem::Window(WindowSpec {
                        func,
                        arg,
                        offset: None,
                        partition_by,
                        order_by,
                    }))
                } else {
                    let agg = AggFunc::from_name(&name).ok_or_else(|| {
                        ParseError::Unexpected(format!("unknown function {name}"))
                    })?;
                    Ok(SelectItem::Agg(agg, arg))
                }
            }
        }
    }

    fn expect_rparen(&mut self) -> Result<()> {
        match self.next()? {
            Token::RParen => Ok(()),
            other => Err(ParseError::Unexpected(format!("{other:?}, expected )"))),
        }
    }

    /// `(PARTITION BY col[,...] ORDER BY col [ASC|DESC][,...])`, both parts optional.
    fn parse_over_clause(&mut self) -> Result<OverClause> {
        match self.next()? {
            Token::LParen => {}
            other => {
                return Err(ParseError::Unexpected(format!(
                    "{other:?}, expected ( after OVER"
                )))
            }
        }
        let partition_by = if self.peek_keyword("PARTITION") {
            self.next()?;
            self.expect_keyword("BY")?;
            self.parse_ident_list()?
        } else {
            Vec::new()
        };
        let order_by = if self.peek_keyword("ORDER") {
            self.next()?;
            self.expect_keyword("BY")?;
            let mut items = vec![self.parse_order_item()?];
            while matches!(self.peek(), Some(Token::Comma)) {
                self.next()?;
                items.push(self.parse_order_item()?);
            }
            items
        } else {
            Vec::new()
        };
        self.expect_rparen()?;
        Ok((partition_by, order_by))
    }

    fn parse_order_item(&mut self) -> Result<(String, bool)> {
        let column = self.ident()?;
        let descending = if self.peek_keyword("DESC") {
            self.next()?;
            true
        } else if self.peek_keyword("ASC") {
            self.next()?;
            false
        } else {
            false
        };
        Ok((column, descending))
    }

    fn parse_expr(&mut self) -> Result<Expr> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_and()?;
        while self.peek_keyword("OR") {
            self.next()?;
            let rhs = self.parse_and()?;
            lhs = Expr::BinaryOp(Box::new(lhs), BinOp::Or, Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_comparison()?;
        while self.peek_keyword("AND") {
            self.next()?;
            let rhs = self.parse_comparison()?;
            lhs = Expr::BinaryOp(Box::new(lhs), BinOp::And, Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_comparison(&mut self) -> Result<Expr> {
        let lhs = self.parse_additive()?;

        if self.peek_keyword("IN") {
            self.next()?;
            match self.next()? {
                Token::LParen => {}
                other => {
                    return Err(ParseError::Unexpected(format!(
                        "{other:?}, expected ( after IN"
                    )))
                }
            }
            let subquery = self.parse_query()?;
            match self.next()? {
                Token::RParen => {}
                other => {
                    return Err(ParseError::Unexpected(format!(
                        "{other:?}, expected ) closing IN subquery"
                    )))
                }
            }
            return Ok(Expr::InSubquery {
                expr: Box::new(lhs),
                subquery: Box::new(subquery),
            });
        }

        let op = match self.peek() {
            Some(Token::Op(op)) => match op.as_str() {
                "=" => Some(BinOp::Eq),
                "!=" | "<>" => Some(BinOp::Ne),
                "<" => Some(BinOp::Lt),
                "<=" => Some(BinOp::Le),
                ">" => Some(BinOp::Gt),
                ">=" => Some(BinOp::Ge),
                _ => None,
            },
            _ => None,
        };
        if let Some(op) = op {
            self.next()?;
            let rhs = self.parse_additive()?;
            Ok(Expr::BinaryOp(Box::new(lhs), op, Box::new(rhs)))
        } else {
            Ok(lhs)
        }
    }

    fn parse_additive(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Some(Token::Op(op)) if op == "+" => Some(BinOp::Add),
                Some(Token::Op(op)) if op == "-" => Some(BinOp::Sub),
                _ => None,
            };
            match op {
                Some(op) => {
                    self.next()?;
                    let rhs = self.parse_multiplicative()?;
                    lhs = Expr::BinaryOp(Box::new(lhs), op, Box::new(rhs));
                }
                None => return Ok(lhs),
            }
        }
    }

    fn parse_multiplicative(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_primary()?;
        loop {
            let op = match self.peek() {
                Some(Token::Star) => Some(BinOp::Mul),
                Some(Token::Op(op)) if op == "/" => Some(BinOp::Div),
                _ => None,
            };
            match op {
                Some(op) => {
                    self.next()?;
                    let rhs = self.parse_primary()?;
                    lhs = Expr::BinaryOp(Box::new(lhs), op, Box::new(rhs));
                }
                None => return Ok(lhs),
            }
        }
    }

    fn parse_primary(&mut self) -> Result<Expr> {
        match self.next()? {
            Token::Ident(mut name) => {
                while matches!(self.peek(), Some(Token::Dot)) {
                    self.next()?;
                    match self.next()? {
                        Token::Ident(part) => {
                            name.push('.');
                            name.push_str(&part);
                        }
                        other => {
                            return Err(ParseError::Unexpected(format!(
                                "{other:?}, expected identifier after '.'"
                            )))
                        }
                    }
                }
                Ok(Expr::Column(name))
            }
            Token::Int(n) => Ok(Expr::Literal(Literal::Int(n))),
            Token::Float(n) => Ok(Expr::Literal(Literal::Float(n))),
            Token::Str(s) => Ok(Expr::Literal(Literal::Str(s))),
            Token::LParen => {
                let expr = self.parse_expr()?;
                match self.next()? {
                    Token::RParen => Ok(expr),
                    other => Err(ParseError::Unexpected(format!("{other:?}, expected )"))),
                }
            }
            other => Err(ParseError::Unexpected(format!(
                "{other:?}, expected expression"
            ))),
        }
    }
}

pub fn parse(input: &str) -> Result<Query> {
    let tokens = tokenize(input)?;
    let mut parser = Parser { tokens, pos: 0 };
    parser.parse_top_level_query()
}

/// Parses `EXPLAIN [QUERY PLAN] <select>`, returning whether the `EXPLAIN`
/// prefix was present along with the parsed query.
pub fn parse_explain(input: &str) -> Result<(bool, Query)> {
    let tokens = tokenize(input)?;
    let mut parser = Parser { tokens, pos: 0 };
    let explain = if parser.peek_keyword("EXPLAIN") {
        parser.next()?;
        if parser.peek_keyword("QUERY") {
            parser.next()?;
            parser.expect_keyword("PLAN")?;
        }
        true
    } else {
        false
    };
    let query = parser.parse_top_level_query()?;
    Ok((explain, query))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sql_expr::{Join, JoinKind};

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
        let err = parse("SELECT id FROM t GARBAGE").unwrap_err();
        assert!(matches!(err, ParseError::Unexpected(_)));
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
            parse("SELECT id FROM orders WHERE region_key IN (SELECT key FROM regions)").unwrap();
        let Some(Expr::InSubquery { expr, subquery }) = q.where_clause else {
            panic!("expected InSubquery")
        };
        assert_eq!(*expr, Expr::Column("region_key".into()));
        assert_eq!(subquery.from, "regions");
        assert_eq!(subquery.columns, vec![SelectItem::Column("key".into())]);
    }

    #[test]
    fn parses_row_number_window() {
        let q = parse("SELECT ROW_NUMBER() OVER (PARTITION BY region ORDER BY id DESC) FROM t")
            .unwrap();
        assert_eq!(
            q.columns,
            vec![SelectItem::Window(WindowSpec {
                func: WindowFunc::RowNumber,
                arg: None,
                offset: None,
                partition_by: vec!["region".into()],
                order_by: vec![("id".into(), true)],
            })]
        );
    }

    #[test]
    fn parses_lag_with_offset() {
        let q =
            parse("SELECT LAG(amount, 2) OVER (PARTITION BY region ORDER BY id) FROM t").unwrap();
        assert_eq!(
            q.columns,
            vec![SelectItem::Window(WindowSpec {
                func: WindowFunc::Lag,
                arg: Some("amount".into()),
                offset: Some(2),
                partition_by: vec!["region".into()],
                order_by: vec![("id".into(), false)],
            })]
        );
    }

    #[test]
    fn sum_without_over_is_still_a_plain_aggregate() {
        let q = parse("SELECT SUM(amount) FROM t").unwrap();
        assert_eq!(
            q.columns,
            vec![SelectItem::Agg(AggFunc::Sum, Some("amount".into()))]
        );
    }
}
