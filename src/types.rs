//! Base value and literal types shared across the t-rust-db SQL layer.
//!
//! `Literal` is the AST-level representation of a literal token as parsed
//! from SQL text (see `sql-parser`). `Value` is the runtime representation
//! used by executors once a literal (or a computed result) needs to carry
//! a "no value" state that a literal never does.

#![forbid(unsafe_code)]

#[derive(Debug, Clone, PartialEq)]
/// A literal as it appears in SQL text; unlike [`Value`] it can never be NULL.
pub enum Literal {
    /// A 64-bit integer literal.
    Int(i64),
    /// A 64-bit floating-point literal.
    Float(f64),
    /// A string literal.
    Str(String),
}

#[derive(Debug, Clone, PartialEq)]
/// A runtime value produced by execution: a [`Literal`]'s payload or SQL NULL.
pub enum Value {
    /// A 64-bit integer.
    Int(i64),
    /// A 64-bit float.
    Float(f64),
    /// A string.
    Str(String),
    /// SQL NULL: the absence of a value.
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
