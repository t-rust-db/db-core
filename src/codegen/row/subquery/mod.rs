//! Subquery codegen (db-core#95) -- ported from sqlite-rs's
//! `src/codegen/subquery.rs` and its `subquery/{scalar,from_clause,
//! flatten,pushdown}.rs`.
//!
//! Materialization only (no coroutines), exactly like the reference:
//! each subquery occurrence opens its own table cursor (and, for `IN`
//! and a `FROM`-subquery, an ephemeral one) via
//! [`super::RegAlloc::alloc_cursor`], compiles the inner `SELECT`'s own
//! single-table scan inline into the enclosing instruction stream, and
//! either tests row existence ([`scalar::compile_exists`]), tests row
//! membership ([`scalar::compile_in_subquery`]), or populates a scannable
//! ephemeral table ([`from_clause::materialize_from_subquery`]).
//!
//! Correlation works for free under materialization: the subquery's own
//! [`super::Scope`] is built with [`super::Scope::with_outer`] pointing
//! at the enclosing scope, so [`super::Scope::resolve`] falls back there
//! for any reference the subquery's own table doesn't resolve. Because
//! the whole `compile_*` call is inlined at the point the subquery
//! expression is evaluated (once per outer row), the outer cursor is
//! already positioned on the current row every time this code runs.
//!
//! **Scoped down from the reference**, in each case because db-core's
//! narrower `Expr`/`Query` cannot express the input:
//!
//! - **Scalar subqueries in value position** (the reference's
//!   `compile_scalar_subquery`) have no [`crate::expr::Expr`] variant to
//!   compile from -- db-core's `Expr` has `InSubquery`/`Exists` and no
//!   bare `Subquery`. `InSubquery`/`Exists` in a *value* context still
//!   work, via `value.rs`'s existing three-valued
//!   condition-to-register materialization.
//! - **Multi-column `IN`** (`compile_in_subquery_multi`): `Expr::
//!   InSubquery`'s left-hand side is a single `Expr`, so there is no
//!   tuple form to compile.
//! - **The `SeekRowid`/`SeekIndexEq` point-lookup fast path** the
//!   reference takes for a correlated equality (its `choose_join_access`)
//!   needs the join-access chooser db-core defers to #117 along with
//!   `planner::Stats`; both `compile_exists` and `compile_in_subquery`
//!   here always compile the plain `Rewind`/`Next` scan the reference
//!   falls back to.
//! - **`hoist_uncorrelated_where_subqueries` (the reference's
//!   `correlation.rs`) and `memoize.rs`** both key a per-statement cache
//!   off `Scope`, which db-core's `Scope` (cloned per nesting level, not
//!   threaded as one mutable pass) has no place to hold. Both are pure
//!   optimizations over the codegen here, so they are deferred rather
//!   than scoped down.
//! - **CTEs** (`WITH ... AS (...)`) and views: neither `Query` nor the
//!   `column` parser has a `with_clause`, and a CTE referenced N times
//!   needs the reference's structural-equality materialization cache
//!   (`RegAlloc::cached_cte`/`OpenDup`) to avoid re-running the body per
//!   reference. Follow-up ticket.
//!
//! [`flatten`] and [`pushdown`] are AST rewrites rather than codegen, and
//! are ported at db-core's own scope: a single `FROM` item, no aliasing
//! beyond the subquery's own mandatory one. The reference's multi-way
//! join-tree cases (pushing into a `JOIN`ed subquery, flattening one of
//! N `FROM` items) have no counterpart while `Query.from` holds exactly
//! one item.

pub mod flatten;
pub mod from_clause;
pub mod pushdown;
pub mod scalar;

pub use flatten::flatten_from_subquery;
pub use from_clause::{materialize_from_subquery, resolve_from_table_schema};
pub use pushdown::push_down_where_predicates;
pub use scalar::{compile_exists, compile_in_subquery};
