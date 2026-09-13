// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Compiling and evaluating a bare boolean expression against one already-
//! materialized row, without a table scan (#369): db-studio's highlight/
//! dim/filter feature needs to test a `WHERE`-grammar expression against a
//! row it already has (as [`Cell`]s keyed by column name) many times over,
//! without re-opening or re-querying the file.
//!
//! Reuses the same grammar and compiler `WHERE` itself uses --
//! [`crate::parser::row::parse_bool_expr`] and
//! [`crate::codegen::batch::compile_bool_expr`] -- rather than a second
//! expression engine: [`CompiledPredicate::eval`] wraps the row in a
//! one-row [`Batch`]/[`InMemorySegment`] and drives it through the same
//! [`vm::engine::run`] every `WHERE` clause runs through, so `LIKE`/`GLOB`
//! (#352) and anything else `WHERE` accepts work here for free.
//!
//! [`vm::engine::run`]: crate::vm::engine::run

use crate::codegen::batch::{bool_expr_columns, compile_bool_expr};
use crate::engine::{Cell, EngineError, ErrorKind};
use crate::parser::row::{parse_bool_expr, ParseOutcome};
use crate::vm::batch::{Batch, Program, Value};
use crate::vm::engine::{run, InMemorySegment};

/// A boolean expression, compiled once against a known set of columns,
/// ready to [`eval`](CompiledPredicate::eval) repeatedly against
/// individual rows.
#[derive(Debug, Clone)]
pub struct CompiledPredicate {
    program: Program,
    /// Columns `expr` actually references -- `eval` requires each of
    /// these be present in the row it's given, so a row missing one is a
    /// clear error rather than a silent `false`.
    required_columns: Vec<String>,
}

impl CompiledPredicate {
    /// Parses and compiles `expr` -- the same grammar `WHERE` uses --
    /// validated against `schema_columns` (the engine's current columns):
    /// a name `expr` references that isn't in `schema_columns` is a
    /// [`ErrorKind::Compile`] error, not a silent `false` at eval time.
    pub fn compile(expr: &str, schema_columns: &[String]) -> Result<Self, EngineError> {
        let parsed = match parse_bool_expr(expr) {
            ParseOutcome::Accepted(expr) => *expr,
            ParseOutcome::Unsupported { message, .. } => {
                return Err(EngineError::new(ErrorKind::Unsupported, message))
            }
            ParseOutcome::Invalid { message, .. } => {
                return Err(EngineError::new(ErrorKind::Parse, message))
            }
        };

        let required_columns = bool_expr_columns(&parsed);
        for name in &required_columns {
            if !schema_columns.iter().any(|c| c == name) {
                return Err(EngineError::new(
                    ErrorKind::Compile,
                    format!("unknown column: {name}"),
                ));
            }
        }

        Ok(CompiledPredicate {
            program: compile_bool_expr(&parsed),
            required_columns,
        })
    }

    /// Evaluates the compiled expression against one row: `row[i]` is the
    /// value of `columns[i]`. Purely a function of `row` and the compiled
    /// program -- no file I/O, no engine re-query. A column the compiled
    /// expression needs but `columns` doesn't carry is an
    /// [`ErrorKind::Compile`] error, not a silent `false`.
    pub fn eval(&self, row: &[Cell], columns: &[String]) -> Result<bool, EngineError> {
        let mut batch = Batch::new(1);
        for name in &self.required_columns {
            let idx = columns.iter().position(|c| c == name).ok_or_else(|| {
                EngineError::new(ErrorKind::Compile, format!("unknown column: {name}"))
            })?;
            let cell = row.get(idx).cloned().unwrap_or(Cell::Null);
            batch = batch.with_column(name.clone(), vec![Value::from(cell)]);
        }

        let segments = [InMemorySegment(batch)];
        let rows = run(&segments, &self.program)
            .map_err(|err| EngineError::new(ErrorKind::Execute, err))?;
        Ok(!rows.is_empty())
    }
}
