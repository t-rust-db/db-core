//! sqlite-rs-style codegen -- one of `sql-codegen`'s three emitters (see
//! crate root docs).
//!
//! **Not yet implemented.** Blocked on [`sql_vm::row`] (db-core#18)
//! existing for real: codegen for a VM that doesn't exist yet in
//! `db-core` has nothing concrete to target (a sqlite-rs-style emitter
//! could target sqlite-rs's own private `Program`/VDBE bytecode type
//! instead, but then it isn't `sql-vm`'s `row` executor this crate exists
//! to pair with -- see db-core#20's own "Additional Notes"). Sequence
//! this after #18, not before or alongside.
//!
//! What "done" looks like is not a guess: sqlite-rs's own
//! `src/codegen/*` is 20,548 lines across these real submodules --
//!
//! - `analyze.rs` -- `ANALYZE` statement codegen.
//! - `ddl.rs` + `ddl/{create_index,create_table,create_view,drop_index,
//!   drop_table}.rs` -- DDL statement codegen.
//! - `dispatch.rs` -- top-level statement-kind dispatch (this crate's
//!   [`super::batch::render_flat`]/`render_joined`/etc. dispatch is the
//!   closest existing analogue, at a fraction of the size).
//! - `expr.rs` + `expr/{cond,value}.rs` -- expression codegen.
//! - `index_maintenance.rs` -- keeping indexes in sync with DML.
//! - `pragma.rs` -- `PRAGMA` statement codegen.
//! - `select.rs` + `select/{aggregate.rs`, `aggregate/`, `entry.rs`,
//!   `eqp.rs`, `index_scan.rs`, `join_access.rs`, `join_full.rs`,
//!   `join_order.rs`, `joins.rs`, `joins/`, `limit_scan.rs`,
//!   `order_by.rs`, `projection.rs`, `range_scan.rs}` -- `SELECT`
//!   codegen, by far the largest and most deeply nested submodule (query
//!   planning, join ordering, index selection, aggregation).
//! - `stmt.rs` + `stmt/{delete,insert,update}.rs` -- DML statement
//!   codegen.
//! - `subquery.rs` + `subquery/{correlation,cte,flatten,from_clause,
//!   memoize,pushdown,scalar,views}.rs` -- subquery handling (CTEs,
//!   correlated subqueries, flattening, materialization).
//! - `transaction.rs` -- `BEGIN`/`COMMIT`/`ROLLBACK` codegen.
//!
//! This is a mechanical port target the same way `sql_parser::row` was
//! (db-core#23) -- migrated in unchanged once there's a real
//! `sql_vm::row::Opcode` to emit, not an independent reimplementation.
//! Given the size, expect this to become several sub-tickets once #18
//! lands, not one PR (matching db-core#20's own "Additional Notes").
