//! sqlite-rs-style planner -- one of `codegen`'s three planners (see
//! module docs).
//!
//! **Started (db-core#91, tracking issue db-core#20).** Ported from
//! sqlite-rs's `src/codegen.rs` + `src/codegen/expr.rs` +
//! `src/codegen/expr/{cond,value}.rs`, targeting
//! [`crate::vm::row::Opcode`] directly (the #18 decision) -- expressions
//! compile to jump-based control flow, never an intermediate boolean
//! register, exactly like the oracle.
//!
//! **Scoped down from a byte-faithful port** (see #91's comment
//! recording this decision): sqlite-rs's `Scope`/`RegAlloc` carry a full
//! table catalog, `ANALYZE` stats, join bindings, and correlated-
//! subquery hoisting/memoization caches -- none of which db-core has
//! yet (they belong to #92 joins / #95 subqueries), and none of which
//! db-core's current [`crate::expr::Expr`] (no `Case`/`Cast`/`Like`/
//! `Between`/`Exists`/qualified columns, unlike sqlite-rs's richer
//! `parser::ast::Expr`) could exercise anyway. This module ports the
//! *mechanism* -- [`Label`]/[`Target`]/[`NullTarget`]/[`CondTargets`]/
//! [`Emitter`], a plain-bump [`RegAlloc`] (no CTE cache, no
//! subquery-cursor allocation), and a single-table [`Scope`] (bare
//! column name to index, no catalog/joins/hoisting) -- sized to what
//! `Expr` has today. #95 grows [`RegAlloc`] a cursor allocator and
//! [`Scope`] a catalog plus an `outer` link, which is what subquery
//! materialization needs; the reference's hoisting/memoization caches
//! are still unported (see [`subquery`]'s own doc).
//!
//! [`TableSchema`] here is a placeholder: just enough (declared column
//! types, one optional rowid-alias column) for [`value::compile_value`]'s
//! column-read/affinity logic to be correct. It is not a real catalog --
//! #118 is expected to replace it with one once a real N-way join needs
//! multi-table resolution.
//!
//! [`select::compile_select`] (db-core#92) covers a single-table scan
//! plus projection (bare columns, `SELECT *`) plus `WHERE` plus
//! `LIMIT`. [`select::compile_select_join`] (db-core#102/#101) adds a
//! single `INNER`/`LEFT`/`FULL` equi-join (a two-pass nested loop for
//! `FULL`'s both-sides null-extension) and `ORDER BY` via the existing
//! single-key sorter, both without any stats/access-path cost model;
//! that chooser and N-way joins are deferred to #117 (needs
//! `planner::Stats`, blocked on #116's missing `ANALYZE` VM
//! implementation) and #118 (no consumer needs N-way joins yet).
//!
//! **#93 adds `GROUP BY`/`HAVING`/aggregation**: [`aggregate`], ported
//! from sqlite-rs's `codegen/select/aggregate.rs` and its
//! `aggregate/{accum,hash,join}.rs`. Both of that module's grouping
//! strategies are ported (sort-then-group over `Sorter*`, and the
//! single-pass hash one over #86's `HashAgg*` slice), plus `HAVING` and
//! aggregation over a join. See that module's own doc for what db-core's
//! narrower `Query`/`SelectItem` scopes out of the reference.
//!
//! **#94 adds index-aware access paths**: [`index_scan`]
//! (index-ordered scans, ADR 0020), [`range_scan`] (index range seeks,
//! ADR 0034), [`limit_scan`] (`LIMIT`/`OFFSET`, and `Query` grows the
//! matching `offset` field), and [`eqp`] (`EXPLAIN QUERY PLAN`). Each
//! is scoped to the case that needs no cost model -- "is there an index
//! whose leading column matches?" -- with the reference's stats-driven
//! parts (skip-scan, `compile_direct_scan`'s chooser, EQP's
//! `stats_by_table`) left unported for the same reason `planner.rs`
//! below is. See each module's own doc.
//!
//! **#97 adds the DDL/transaction/PRAGMA/ANALYZE slice**: [`ddl`]
//! (`CREATE`/`DROP TABLE`/`INDEX`/`VIEW`), [`transaction`]
//! (`BEGIN`/`COMMIT`/`ROLLBACK`), [`pragma`] (`journal_mode`/
//! `integrity_check`/`synchronous`), [`analyze`] (`ANALYZE` into
//! `sqlite_stat1`), and [`dispatch`] (the statement dispatcher). Each of
//! these compiles to a single procedural opcode at exec time -- no
//! per-row cursor work, unlike the expression-driven DML codegen #91
//! started -- so [`TableSchema`] grows `root_page`/`indexes` here
//! (via the new [`IndexSchema`]) purely to bake root pages/names into
//! `P4` at codegen time, the same way sqlite-rs's schema catalog does.
//!
//! [`dispatch::compile_statement`] only routes the statement kinds this
//! module (and #91's expr slice) actually have codegen for --
//! `BEGIN`/`COMMIT`/`ROLLBACK`/`PRAGMA`/`ANALYZE`/`CREATE TABLE`/
//! `CREATE INDEX`/`CREATE VIEW`/`DROP TABLE`/`DROP INDEX`. sqlite-rs's
//! own `dispatch.rs` also routes `INSERT`/`UPDATE`/`DELETE`/`SELECT`,
//! but those have no codegen counterpart in `db-core` yet, so routing
//! them is deferred to whichever sub-ticket of #20 ports that codegen.
//!
//! **#95 adds subqueries**: [`subquery`], ported from sqlite-rs's
//! `codegen/subquery.rs` and its `subquery/{scalar,from_clause,flatten,
//! pushdown}.rs`. `Expr` grows an `Exists` variant and `Query.from`
//! becomes a [`crate::expr::FromClause`] (a table name or a subquery
//! plus its alias), and [`select::compile_select_with_catalog`] is the
//! entry point that wires the cursors itself, since a subquery's own
//! `FROM` table can't be pre-wired by the caller. See that module's own
//! doc for what db-core's narrower `Expr`/`Query` scopes out of the
//! reference -- notably CTEs and the correlated-subquery
//! hoisting/memoization caches.
//!
//! **`planner.rs` (sqlite-rs's 386-line cost model) is deliberately not
//! ported here yet**, even though `codegen::row::planner` is its
//! natural home (per #97's own note): it decodes `sqlite_stat1` via
//! real b-tree/page-source types (`crate::btree`/`crate::vfs` in
//! sqlite-rs) and feeds a join-access chooser (`join_access`) that
//! db-core has neither of yet (storage integration is #18's `vm-row`
//! feature; joins are #92). Porting it now would mean vendoring dead
//! code with no caller. Tracked as a follow-up once both land.

#![forbid(unsafe_code)]

pub mod aggregate;
pub mod analyze;
pub mod cond;
pub mod ddl;
pub mod dispatch;
pub mod eqp;
pub mod index_maintenance;
pub mod index_scan;
pub mod limit_scan;
pub mod pragma;
pub mod range_scan;
pub mod select;
pub mod stmt;
pub mod subquery;
pub mod transaction;
pub mod value;

use std::collections::HashMap;
use std::fmt;

use crate::vm::row::{Instruction, Opcode, P4};

pub use analyze::compile_analyze;
pub use cond::compile_cond;
pub use ddl::{
    compile_create_index, compile_create_table, compile_create_view, compile_drop_index,
    compile_drop_table,
};
pub use eqp::{explain_query_plan, EqpRow};
pub use pragma::compile_pragma;
pub use select::{compile_select, compile_select_join, compile_select_with_catalog};
pub use stmt::{compile_delete, compile_insert, compile_update};
pub use subquery::{
    compile_exists, compile_in_subquery, flatten_from_subquery, materialize_from_subquery,
    push_down_where_predicates, resolve_from_table_schema,
};
pub use transaction::{compile_begin, compile_commit, compile_rollback};
pub use value::compile_value;

/// The nesting bound this compiler enforces while walking an [`Expr`]
/// tree, mirroring sqlite-rs's ADR 0014 (200, deliberately below
/// SQLite's own 1000 -- a debug-build stack-overflow investigation
/// found the guard was otherwise unreachable at this crate's real
/// recursion depth per `Expr` level).
pub const MAX_EXPR_DEPTH: usize = 200;

/// Codegen failures: an unresolvable column, an `Expr` tree deeper than
/// [`MAX_EXPR_DEPTH`], or a construct this scoped-down compiler doesn't
/// implement yet (see this module's doc comment).
#[derive(Debug, Clone, PartialEq)]
pub enum CodegenError {
    UnknownColumn(String),
    TooDeep,
    Unsupported { reason: String },
}

impl fmt::Display for CodegenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodegenError::UnknownColumn(name) => write!(f, "unknown column: {name}"),
            CodegenError::TooDeep => {
                write!(f, "expression nesting exceeds {MAX_EXPR_DEPTH} levels")
            }
            CodegenError::Unsupported { reason } => write!(f, "unsupported: {reason}"),
        }
    }
}

impl std::error::Error for CodegenError {}

pub type Result<T> = std::result::Result<T, CodegenError>;

/// A not-yet-resolved jump target, placed later via [`Emitter::place`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Label(usize);

/// Where a boolean condition's true/false outcome continues: either an
/// explicit jump target, or "fall through to the next emitted
/// instruction" -- the classic jumping-code-generation technique, used
/// throughout this module so AND/OR compose without materializing an
/// intermediate boolean register.
#[derive(Debug, Clone, Copy)]
pub enum Target {
    Jump(Label),
    Fallthrough,
}

/// Where a condition's *unknown* (SQL NULL) outcome continues -- names
/// one of [`CondTargets`]'s other two targets rather than being a third
/// [`Target`] of its own; see sqlite-rs's `NullTarget` doc for why an
/// independent third continuation doesn't work for `AND`/`OR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullTarget {
    /// NULL continues where [`CondTargets::on_true`] does.
    True,
    /// NULL continues where [`CondTargets::on_false`] does -- what
    /// `WHERE` wants.
    False,
}

/// The full jump-mode contract: where a condition's true, false, and
/// unknown outcomes each continue. Bundled rather than three parameters
/// because [`CondTargets::negate`] has to move all three together --
/// swapping true and false without flipping `on_null` reproduces
/// sqlite-rs's #134 bug.
#[derive(Debug, Clone, Copy)]
pub struct CondTargets {
    pub on_true: Target,
    pub on_false: Target,
    pub on_null: NullTarget,
}

impl CondTargets {
    /// The setting every boolean consumer here wants: unknown joins
    /// false.
    pub fn null_is_false(on_true: Target, on_false: Target) -> Self {
        CondTargets {
            on_true,
            on_false,
            on_null: NullTarget::False,
        }
    }

    /// Unknown joins true -- used only to separate "definitely false"
    /// from "unknown" when materializing a condition into a register.
    pub fn null_is_true(on_true: Target, on_false: Target) -> Self {
        CondTargets {
            on_true,
            on_false,
            on_null: NullTarget::True,
        }
    }

    /// The contract for the operand of a `NOT`: true and false trade
    /// places, and `on_null` flips so the unknown outcome still names
    /// the address it named before the swap.
    pub fn negate(self) -> Self {
        CondTargets {
            on_true: self.on_false,
            on_false: self.on_true,
            on_null: match self.on_null {
                NullTarget::True => NullTarget::False,
                NullTarget::False => NullTarget::True,
            },
        }
    }

    pub fn with_true(self, on_true: Target) -> Self {
        CondTargets { on_true, ..self }
    }

    pub fn with_false(self, on_false: Target) -> Self {
        CondTargets { on_false, ..self }
    }
}

/// Builds a [`crate::vm::row::Program`] with forward-referenceable jump
/// targets: `new_label`/`place` mark an address, `patch_p2` records a
/// pending fixup (every jump-carrying opcode this module emits targets
/// `p2`), and [`Emitter::finish`] resolves every pending fixup in one
/// pass.
#[derive(Debug, Default)]
pub struct Emitter {
    instructions: Vec<Instruction>,
    labels: HashMap<Label, usize>,
    patches: Vec<(usize, Label)>,
    next_label: usize,
}

impl Emitter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn emit(&mut self, instr: Instruction) -> usize {
        self.instructions.push(instr);
        self.instructions.len().saturating_sub(1)
    }

    pub fn here(&self) -> usize {
        self.instructions.len()
    }

    pub fn new_label(&mut self) -> Label {
        let label = Label(self.next_label);
        self.next_label = self.next_label.saturating_add(1);
        label
    }

    /// Binds `label` to the current (next-to-be-emitted) address.
    pub fn place(&mut self, label: Label) {
        self.labels.insert(label, self.here());
    }

    pub fn patch_p2(&mut self, addr: usize, label: Label) {
        self.patches.push((addr, label));
    }

    /// Resolves every pending patch against its placed label's address,
    /// consuming the emitter into a finished [`crate::vm::row::Program`].
    pub fn finish(mut self) -> crate::vm::row::Program {
        for (addr, label) in &self.patches {
            let Some(&resolved) = self.labels.get(label) else {
                continue; // Every patched label is always placed by construction; skip defensively rather than panic.
            };
            #[allow(clippy::cast_possible_wrap)]
            let target = resolved as i32;
            if let Some(instr) = self.instructions.get_mut(*addr) {
                instr.p2 = target;
            }
        }
        crate::vm::row::Program::new(self.instructions)
    }

    /// Emits an unconditional jump to `label`, patched once placed.
    pub fn goto(&mut self, label: Label) {
        let addr = self.emit(Instruction::new(Opcode::Goto, 0, 0, 0));
        self.patch_p2(addr, label);
    }
}

/// A monotonically-increasing register bump allocator -- the simplest
/// correct scheme for this scoped-down compiler; a real allocator that
/// reuses freed slots is deferred to whichever sub-ticket actually
/// needs it.
#[derive(Debug, Default)]
pub struct RegAlloc {
    next: i32,
    next_cursor: i32,
    /// A structural-equality cache of every `FROM`-subquery this
    /// compile has already materialized, keyed by the subquery's own
    /// AST (db-core#143) -- mirrors sqlite-rs's `RegAlloc::cached_cte`.
    /// See [`crate::codegen::row::subquery::materialize_from_subquery`]'s
    /// doc for why raw structural equality is safe today (no
    /// correlated variables, no volatile expression reaches this path
    /// yet) and what has to change the day that stops being true.
    cte_cache: Vec<(crate::expr::Query, i32, TableSchema)>,
}

impl RegAlloc {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn alloc(&mut self) -> i32 {
        let r = self.next;
        self.next = self.next.saturating_add(1);
        r
    }

    /// Hands out a fresh cursor slot (db-core#95): every subquery
    /// occurrence opens its own table cursor, plus an ephemeral one for
    /// `IN`'s membership index or a `FROM`-subquery's materialized
    /// result. Mirrors sqlite-rs's `RegAlloc::alloc_cursor`.
    pub fn alloc_cursor(&mut self) -> i32 {
        let c = self.next_cursor;
        self.next_cursor = self.next_cursor.saturating_add(1);
        c
    }

    /// Moves the cursor bump allocator past `cursor`, so a later
    /// [`RegAlloc::alloc_cursor`] can't hand back a slot the caller
    /// wired up by hand (`select.rs` derives its sorter/index cursors by
    /// arithmetic rather than through this allocator).
    pub fn reserve_cursors_through(&mut self, cursor: i32) {
        self.next_cursor = self.next_cursor.max(cursor.saturating_add(1));
    }

    /// The register the next [`RegAlloc::alloc`] call would hand out,
    /// without allocating it.
    pub fn peek(&self) -> i32 {
        self.next
    }

    /// Looks up an already-materialized subquery structurally equal to
    /// `subquery`, returning the cursor it was materialized onto and its
    /// synthetic schema, for the caller to `OpenDup` instead of
    /// re-running the same query. Mirrors sqlite-rs's
    /// `RegAlloc::cached_cte`.
    pub fn cached_cte(&self, subquery: &crate::expr::Query) -> Option<(i32, TableSchema)> {
        self.cte_cache
            .iter()
            .find(|(q, _, _)| q == subquery)
            .map(|(_, cursor, schema)| (*cursor, schema.clone()))
    }

    /// Records that `subquery` was materialized onto `cursor` with
    /// `schema`, so a later structurally-identical occurrence can reuse
    /// it via [`RegAlloc::cached_cte`]. Mirrors sqlite-rs's
    /// `RegAlloc::cache_cte`.
    pub fn cache_cte(&mut self, subquery: crate::expr::Query, cursor: i32, schema: TableSchema) {
        self.cte_cache.push((subquery, cursor, schema));
    }
}

pub(crate) fn p4_coll_seq(
    collation: crate::vm::row::Collation,
    affinity: crate::vm::row::Affinity,
) -> P4 {
    P4::CollSeq {
        collation,
        affinity: affinity.to_p4_byte(),
    }
}

/// A placeholder single-table schema -- see this module's doc comment
/// for why it isn't a real catalog yet.
#[derive(Debug, Clone, Default)]
pub struct TableSchema {
    pub name: String,
    pub columns: Vec<String>,
    /// Declared type string per column, parallel to `columns` -- used
    /// only for [`crate::vm::row::affinity_of`]. A missing/short entry
    /// is treated as no declared type (BLOB affinity).
    pub column_types: Vec<String>,
    /// The `INTEGER PRIMARY KEY` rowid-alias column, if any -- reading
    /// it must emit `Opcode::Rowid` rather than `Opcode::Column` (see
    /// [`value::emit_column_read`]'s doc comment).
    pub rowid_alias: Option<usize>,
    /// The table b-tree's root page (db-core#97's DDL/ANALYZE slice
    /// needs this to bake root pages into `P4` at codegen time; the
    /// expr-only slice from #91 never read it).
    pub root_page: u32,
    /// Every index on this table (db-core#97/#96) -- maintained by
    /// `INSERT`/`UPDATE`/`DELETE` codegen (see [`index_maintenance`]),
    /// not yet consulted by any scan (that's `#94`'s index-scan
    /// codegen, a separate ticket).
    pub indexes: Vec<IndexSchema>,
}

impl TableSchema {
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(name))
    }
}

/// An index descriptor: just enough for `ddl`/`analyze` (db-core#97) to
/// bake index identity into `P4` at codegen time, and for
/// `INSERT`/`UPDATE`/`DELETE` (db-core#96) to build/maintain its
/// entries. Not a real catalog entry: no collation or partial-index
/// predicate, and `columns` is ascending-only (no per-column `DESC`,
/// since no index b-tree comparator here is aware of sort direction
/// either).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IndexSchema {
    pub name: String,
    pub root_page: u32,
    pub columns: Vec<String>,
}

/// The table(s) a query's column references resolve against. Single-table
/// queries never need to disambiguate, since db-core's current
/// [`crate::expr::Expr::Column`] has no qualifier of its own -- qualifiers
/// only ever arrive as a `"table.column"`-shaped plain string. #102 grows
/// this to an optional second (right-side) binding for a single equi-join,
/// and #95 a `catalog`/`outer` pair for subqueries; full multi-table
/// resolution (N-way joins) is still deferred to #101.
#[derive(Debug, Clone)]
pub struct Scope {
    pub schema: TableSchema,
    pub cursor: i32,
    /// The join's right-hand table, when this scope covers a join.
    pub right: Option<(TableSchema, i32)>,
    /// Every table a subquery nested in this scope may name in its own
    /// `FROM` (db-core#95). Empty for a caller that wired its cursors up
    /// by hand and has no subquery to resolve -- the same reason
    /// sqlite-rs's `Scope` carries a catalog while db-core's didn't.
    pub catalog: Vec<TableSchema>,
    /// The enclosing query's scope, for a correlated subquery
    /// (db-core#95): [`Scope::resolve`] falls back here for any
    /// reference this scope's own tables don't resolve, which is all
    /// correlation needs under materialization -- the outer cursor is
    /// already positioned on the current row every time the inlined
    /// subquery code runs.
    pub outer: Option<Box<Scope>>,
}

impl Scope {
    pub fn single(schema: TableSchema, cursor: i32) -> Self {
        Scope {
            schema,
            cursor,
            right: None,
            catalog: Vec::new(),
            outer: None,
        }
    }

    pub fn with_catalog(mut self, catalog: Vec<TableSchema>) -> Self {
        self.catalog = catalog;
        self
    }

    pub fn with_outer(mut self, outer: Scope) -> Self {
        self.outer = Some(Box::new(outer));
        self
    }

    /// Looks `name` up in `catalog`, case-insensitively by table name.
    pub fn catalog_table(&self, name: &str) -> Option<&TableSchema> {
        self.catalog
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(name))
    }

    /// A scope over a single equi-join's two tables.
    pub fn join(
        schema: TableSchema,
        cursor: i32,
        right_schema: TableSchema,
        right_cursor: i32,
    ) -> Self {
        Scope {
            schema,
            cursor,
            right: Some((right_schema, right_cursor)),
            catalog: Vec::new(),
            outer: None,
        }
    }

    /// Splits an optional `table.column` qualifier off `name`, mirroring
    /// `codegen::batch::split_qualified`'s convention.
    fn split_qualified(name: &str) -> (Option<&str>, &str) {
        match name.find('.') {
            Some(idx) => (Some(&name[..idx]), &name[idx + 1..]),
            None => (None, name),
        }
    }

    /// Resolves a (possibly `table.column`-qualified) column name to
    /// `(cursor, column_index)`. Following `codegen::batch::compile_join`'s
    /// established convention: an unqualified name always resolves to the
    /// left/`FROM` table; reaching the right table's column requires an
    /// explicit qualifier, sidestepping true ambiguity detection when both
    /// tables share a column name.
    pub fn resolve(&self, name: &str) -> Result<(i32, usize)> {
        match self.resolve_local(name) {
            Some(binding) => Ok(binding),
            None => match &self.outer {
                Some(outer) => outer.resolve(name),
                None => Err(CodegenError::UnknownColumn(name.to_string())),
            },
        }
    }

    /// Resolves against this scope's own table(s) only, without the
    /// [`Scope::outer`] fallback. A qualifier naming neither of this
    /// scope's tables resolves to nothing here rather than being
    /// stripped and matched against the left table anyway -- that's what
    /// lets a correlated `WHERE inner.x = outer.x` reach the enclosing
    /// scope even when both tables happen to have an `x`.
    fn resolve_local(&self, name: &str) -> Option<(i32, usize)> {
        let (qualifier, col) = Self::split_qualified(name);
        if let (Some(q), Some((right_schema, right_cursor))) = (qualifier, &self.right) {
            if q.eq_ignore_ascii_case(&right_schema.name) {
                return right_schema
                    .column_index(col)
                    .map(|idx| (*right_cursor, idx));
            }
        }
        if let Some(q) = qualifier {
            if self.outer.is_some() && !q.eq_ignore_ascii_case(&self.schema.name) {
                return None;
            }
        }
        self.schema.column_index(col).map(|idx| (self.cursor, idx))
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
    fn emitter_resolves_forward_jump() {
        let mut em = Emitter::new();
        let label = em.new_label();
        let addr = em.emit(Instruction::new(Opcode::Goto, 0, 0, 0));
        em.patch_p2(addr, label);
        em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
        em.place(label);
        let program = em.finish();
        assert_eq!(program.instructions[0].p2, 2);
    }

    #[test]
    fn cond_targets_negate_flips_null_target() {
        let true_label = Label(0);
        let false_label = Label(1);
        let targets =
            CondTargets::null_is_false(Target::Jump(true_label), Target::Jump(false_label));
        let negated = targets.negate();
        assert_eq!(negated.on_null, NullTarget::True);
        assert!(matches!(negated.on_true, Target::Jump(l) if l == false_label));
        assert!(matches!(negated.on_false, Target::Jump(l) if l == true_label));
    }

    #[test]
    fn reg_alloc_hands_out_increasing_registers() {
        let mut reg = RegAlloc::new();
        assert_eq!(reg.alloc(), 0);
        assert_eq!(reg.alloc(), 1);
        assert_eq!(reg.peek(), 2);
    }

    #[test]
    fn scope_resolves_known_column_and_rejects_unknown() {
        let schema = TableSchema {
            name: "t".into(),
            columns: vec!["a".into(), "b".into()],
            column_types: vec![String::new(), String::new()],
            rowid_alias: None,
            root_page: 0,
            indexes: vec![],
        };
        let scope = Scope::single(schema, 3);
        assert_eq!(scope.resolve("b"), Ok((3, 1)));
        assert_eq!(scope.resolve("B"), Ok((3, 1)));
        assert_eq!(
            scope.resolve("z"),
            Err(CodegenError::UnknownColumn("z".to_string()))
        );
    }
}
