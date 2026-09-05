//! Base value and literal types shared across the t-rust-db SQL layer.
//!
//! `Literal` is the AST-level representation of a literal token as parsed
//! from SQL text (see `sql-parser`). `Value` is the runtime representation
//! used by executors once a literal (or a computed result) needs to carry
//! a "no value" state that a literal never does.

#![forbid(unsafe_code)]

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Int(i64),
    Float(f64),
    Str(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Str(String),
    Null,
}

impl From<Literal> for Value {
    fn from(lit: Literal) -> Self {
        match lit {
            Literal::Int(n) => Value::Int(n),
            Literal::Float(n) => Value::Float(n),
            Literal::Str(s) => Value::Str(s),
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

    #[test]
    fn literal_converts_to_value() {
        assert_eq!(Value::from(Literal::Int(5)), Value::Int(5));
        assert_eq!(
            Value::from(Literal::Str("x".into())),
            Value::Str("x".into())
        );
    }
}
