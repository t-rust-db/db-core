//! `SELECT` codegen -- see `super`'s module doc.
//!
//! **Single-table scan + projection + `WHERE` + `LIMIT`** (db-core#92),
//! **a single `INNER`/`LEFT`/`FULL` equi-join and `ORDER BY` via the
//! existing single-key sorter** (db-core#102/#101, both without any
//! stats/access-path cost model), and `GROUP BY`/`HAVING`/aggregation
//! via [`super::aggregate`] (db-core#93). The join-order/access-path chooser and real `planner::Stats`
//! (needs a working `ANALYZE` VM implementation, #116) to #117; N-way
//! joins and multi-table catalogs to #118 (no consumer needs them yet);
//! `DISTINCT` is not yet supported.
//!
//! **#94 adds index-aware access paths**: [`super::index_scan`] (an
//! `ORDER BY` an index already satisfies, walked directly instead of
//! sorted), [`super::range_scan`] (a `WHERE`-bounded indexed column,
//! seeked instead of scanned and filtered), [`super::limit_scan`]
//! (`LIMIT`/`OFFSET` counters and guards), and [`super::eqp`]
//! (`EXPLAIN QUERY PLAN`). Both fast paths are tried before the
//! ordinary `Rewind`/`Next` scan and fall back to it whenever their
//! shape doesn't match; neither compares candidate indexes by cost,
//! since that needs the `planner::Stats` deferred with #116/#117 --
//! see each module's own doc for the exact scope.

use super::limit_scan::{self, LimitState};
use super::value::{compile_value, emit_column_read, qualified_name};
use super::{index_scan, range_scan};
use super::{
    CodegenError, CondTargets, Emitter, Label, RegAlloc, Result, Scope, TableSchema, Target,
};
use crate::parser::ast::{
    BinaryOp, Expr, ExprKind, FunctionArgs, Join, JoinConstraint, JoinOp, Literal, ResultColumn,
    Select, TableRefKind,
};
use crate::vm::row::{Collation, Instruction, Opcode, Program, SortKeyColumn, P4};

/// Where an `ORDER BY` term's sort key comes from: a raw column
/// already resolved into `columns` (index into that vector), or a
/// genuine expression that must be compiled into its own register and
/// appended after the row's other columns -- its final record
/// position isn't known until that compile actually happens, since it
/// depends on how many registers the expression itself allocates
/// (mirrors sqlite-rs's `OrderByTarget`, db-core#167).
#[derive(Debug, Clone)]
pub(super) enum OrderByTarget {
    Column(usize),
    Expr(Expr),
}

/// One `ORDER BY` term's resolved sort direction/nulls-ordering, kept
/// separate from the eventual [`SortKeyColumn`] because an
/// [`OrderByTarget::Expr`]'s `index` isn't known at plan time.
#[derive(Debug, Clone)]
pub(super) struct OrderByPlan {
    pub(super) target: OrderByTarget,
    pub(super) descending: bool,
    pub(super) nulls_first: bool,
}

/// One `SELECT`-list item, as resolved for [`compile_row_values`]: a bare
/// column, read straight off a cursor via the existing
/// [`emit_column_read`] fast path (also how `ORDER BY`'s extra sort-key
/// column -- appended when its term isn't already in the projection --
/// is represented, since that trick only ever resolves by name), or an
/// arbitrary expression, compiled through [`compile_value`] like any
/// other value position (db-core#168).
#[derive(Debug, Clone)]
pub(super) enum ProjectedColumn {
    Name(String),
    Expr(Expr),
}

impl ProjectedColumn {
    fn name_eq(&self, other: &str) -> bool {
        matches!(self, ProjectedColumn::Name(n) if n.eq_ignore_ascii_case(other))
    }
}

/// Compiles `query` (a single-table `SELECT`, no `JOIN`) against
/// `schema`, scanning the pre-wired cursor slot `cursor`. A query with a
/// `JOIN` must use [`compile_select_join`] instead, since resolving it
/// needs a second pre-wired cursor this signature has no room for.
pub fn compile_select(schema: &TableSchema, cursor: i32, query: &Select) -> Result<Program> {
    if !super::joins_of(query).is_empty() {
        return Err(CodegenError::Unsupported {
            reason: "SELECT with a JOIN must be compiled via compile_select_join".to_string(),
        });
    }
    compile_select_inner(schema, cursor, None, query, &[], None)
}

/// Compiles `query` (a `SELECT` with a single `INNER`/`LEFT`/`FULL`
/// equi-join) against `schema`/`cursor` (the `FROM` table) joined to
/// `right_schema`/`right_cursor` (`query.joins[0].table`), both
/// pre-wired cursor slots. `Right`/`Cross` joins, more than one `JOIN`,
/// and any join-order/access-path cost model are deferred to #117
/// (needs `planner::Stats`, which db-core doesn't have yet); N-way
/// joins and a multi-table catalog `Scope` to #118 (no consumer needs
/// them yet).
pub fn compile_select_join(
    schema: &TableSchema,
    cursor: i32,
    right_schema: &TableSchema,
    right_cursor: i32,
    query: &Select,
) -> Result<Program> {
    compile_select_inner(
        schema,
        cursor,
        Some((right_schema, right_cursor)),
        query,
        &[],
        None,
    )
}

#[allow(clippy::too_many_lines)]
/// Compiles `query` against a `catalog` of every table it (or any
/// subquery inside it) may name, wiring the cursors itself -- the entry
/// point db-core#95's subqueries need, since a subquery's own `FROM`
/// table can't be pre-wired by a caller that hasn't parsed it.
/// Equivalent to [`compile_select`] for a plain single-table `FROM` with
/// no subquery anywhere.
///
/// Applies [`super::subquery::push_down_where_predicates`] and
/// [`super::subquery::flatten_from_subquery`] to `query` first, in that
/// order -- the same order the reference runs them (a predicate is
/// pushed into a subquery that flattening may then dissolve entirely,
/// leaving the predicate exactly where it would have ended up anyway).
pub fn compile_select_with_catalog(catalog: &[TableSchema], query: &Select) -> Result<Program> {
    let mut query = query.clone();
    super::subquery::expand_with_clause(&mut query)?;
    super::subquery::push_down_where_predicates(&mut query);
    super::subquery::flatten_from_subquery(&mut query);

    let schema = super::subquery::resolve_from_table_schema(query.from.as_ref(), catalog)?;
    // The AST nests the `FROM` table inside an optional `FromClause`
    // and spells a subquery as a `TableRefKind`, where `expr::Query`
    // had a two-variant `FromClause` field that was always present.
    let from_subquery = match query.from.as_ref().map(|from| &from.first.kind) {
        Some(TableRefKind::Subquery(subquery)) => Some(subquery.as_ref().clone()),
        _ => None,
    };
    compile_select_inner(&schema, 0, None, &query, catalog, from_subquery.as_ref())
}

fn compile_select_inner(
    schema: &TableSchema,
    cursor: i32,
    right: Option<(&TableSchema, i32)>,
    query: &Select,
    catalog: &[TableSchema],
    from_subquery: Option<&Select>,
) -> Result<Program> {
    let is_distinct = super::is_distinct(query);
    // `compile_select_with_catalog` already expanded `with_clause` away
    // via `super::subquery::expand_with_clause` before reaching here --
    // a `Some` this far in means the caller went through the pre-wired
    // `compile_select`/`compile_select_join` entry points instead, which
    // have no catalog to resolve a CTE's materialization against.
    if query.with_clause.is_some() {
        return Err(CodegenError::Unsupported {
            reason: "WITH / common table expressions require the compile_select_with_catalog \
                     entry point"
                .to_string(),
        });
    }
    // Compound SELECT has no ticket yet.
    if !query.compound.is_empty() {
        return Err(CodegenError::Unsupported {
            reason: "compound SELECT (UNION/INTERSECT/EXCEPT) is not supported yet".to_string(),
        });
    }
    let joins = super::joins_of(query);
    if joins.len() > 1 {
        return Err(CodegenError::Unsupported {
            reason: "only a single JOIN is supported; N-way joins are deferred to #101".to_string(),
        });
    }
    let join = joins.first();
    match (join, right) {
        (Some(join), Some(_)) => {
            if !matches!(join.op, JoinOp::Inner | JoinOp::Left | JoinOp::Full) {
                return Err(CodegenError::Unsupported {
                    reason: "only INNER/LEFT/FULL JOIN are supported; the join-order/access-path \
chooser is deferred to #117, N-way joins to #118"
                        .to_string(),
                });
            }
        }
        (Some(_), None) => {
            return Err(CodegenError::Unsupported {
                reason: "SELECT with a JOIN must be compiled via compile_select_join".to_string(),
            });
        }
        (None, Some(_)) => {
            return Err(CodegenError::Unsupported {
                reason: "a right cursor was supplied but the query has no JOIN".to_string(),
            });
        }
        (None, None) => {}
    }

    let scope = match right {
        Some((right_schema, right_cursor)) => {
            Scope::join(schema.clone(), cursor, right_schema.clone(), right_cursor)
        }
        None => Scope::single(schema.clone(), cursor),
    }
    .with_catalog(catalog.to_vec());

    if !query.group_by.is_empty() || super::aggregate::query_has_aggregate(query) {
        if is_distinct {
            return Err(CodegenError::Unsupported {
                reason: "DISTINCT combined with GROUP BY/aggregation is not yet supported"
                    .to_string(),
            });
        }
        return compile_aggregate_select(schema, cursor, right, query, &scope);
    }

    let mut columns = Vec::with_capacity(query.columns.len());
    for item in &query.columns {
        match item {
            // `ResultColumn::Expr` covers what `SelectItem::Column` did
            // and much more; a bare column reference still resolves by
            // name (the fast `emit_column_read` path), and anything else
            // compiles through the general expression compiler
            // (db-core#168). An alias is accepted and ignored -- it
            // renames the output, which this planner does not model yet.
            ResultColumn::Expr { expr, .. } => match &expr.kind {
                ExprKind::Column {
                    table: None, name, ..
                } => columns.push(ProjectedColumn::Name(name.clone())),
                ExprKind::Column {
                    table: Some(table),
                    name,
                    ..
                } => columns.push(ProjectedColumn::Name(format!("{table}.{name}"))),
                _ => columns.push(ProjectedColumn::Expr(expr.clone())),
            },
            ResultColumn::Star => {
                columns.extend(schema.columns.iter().cloned().map(ProjectedColumn::Name));
                if let Some((right_schema, _)) = right {
                    columns.extend(
                        right_schema
                            .columns
                            .iter()
                            .map(|c| ProjectedColumn::Name(format!("{}.{c}", right_schema.name))),
                    );
                }
            }
            ResultColumn::TableStar { table } => {
                if table.eq_ignore_ascii_case(&schema.name) {
                    columns.extend(schema.columns.iter().cloned().map(ProjectedColumn::Name));
                } else if let Some((right_schema, _)) = right {
                    if table.eq_ignore_ascii_case(&right_schema.name) {
                        columns.extend(
                            right_schema.columns.iter().map(|c| {
                                ProjectedColumn::Name(format!("{}.{c}", right_schema.name))
                            }),
                        );
                    } else {
                        return Err(CodegenError::Unsupported {
                            reason: format!("`{table}.*` refers to an unknown table"),
                        });
                    }
                } else {
                    return Err(CodegenError::Unsupported {
                        reason: format!("`{table}.*` refers to an unknown table"),
                    });
                }
            }
        }
    }
    // A `SELECT`-list expression makes the index-ordered-scan/range-seek
    // fast paths' `columns: &[ProjectedColumn]` no longer purely
    // name-based; both already fall back to the ordinary scan whenever
    // their own shape doesn't match; treating "any projected expression"
    // the same way is a safe, no-regression fallback -- such queries
    // were rejected outright before #168, so the slower generic path is
    // strictly better, not a loss.
    let has_projected_expr = columns
        .iter()
        .any(|c| matches!(c, ProjectedColumn::Expr(_)));

    // When there's an `ORDER BY`, rows are buffered into a sorter instead
    // of being emitted directly, and `LIMIT` applies to the sorted
    // output rather than scan order -- see `sort_key`/`output_count`
    // below and the drain loop at the end of this function. `columns`
    // gains the sort key as an extra, trailing record column when it
    // isn't already part of the projection; `output_count` stays at the
    // original projection width so the drain loop never emits it.
    let output_count = columns.len();
    // Every `ORDER BY` term becomes one `OrderByPlan`, in source order
    // -- the sorter (`P4::SortKey(Vec<SortKeyColumn>)`) has always taken
    // a vector; only codegen's own plumbing capped it at one (#149). A
    // bare column resolves to a `columns` index right here; anything
    // else becomes an `OrderByTarget::Expr` whose final record position
    // is resolved later, once `emit_row` actually compiles it into a
    // register (db-core#167).
    let sort_key = if query.order_by.is_empty() {
        None
    } else {
        let mut keys = Vec::with_capacity(query.order_by.len());
        for term in &query.order_by {
            let target = match &term.expr.kind {
                ExprKind::Column {
                    table: None, name, ..
                } => OrderByTarget::Column(
                    columns
                        .iter()
                        .position(|c| c.name_eq(name))
                        .unwrap_or_else(|| {
                            let idx = columns.len();
                            columns.push(ProjectedColumn::Name(name.clone()));
                            idx
                        }),
                ),
                ExprKind::Column {
                    table: Some(table),
                    name,
                    ..
                } => {
                    let qualified = format!("{table}.{name}");
                    OrderByTarget::Column(
                        columns
                            .iter()
                            .position(|c| c.name_eq(&qualified))
                            .unwrap_or_else(|| {
                                let idx = columns.len();
                                columns.push(ProjectedColumn::Name(qualified));
                                idx
                            }),
                    )
                }
                _ => OrderByTarget::Expr(term.expr.clone()),
            };
            let descending = term.desc.unwrap_or(false);
            // SQLite's default (unstated `NULLS FIRST`/`LAST`) is NULLS
            // FIRST for a `DESC` term and NULLS LAST for `ASC`; an
            // explicit clause overrides that default either way.
            let nulls_first = term.nulls_last.map_or(descending, |last| !last);
            keys.push(OrderByPlan {
                target,
                descending,
                nulls_first,
            });
        }
        Some(keys)
    };
    // `DISTINCT` (db-core#176) reuses the sorter: every output column
    // joins the sort key (appended after any `ORDER BY` terms, harmless
    // duplication when a term already names one) so two rows compile to
    // the same projection if and only if they sort adjacent to each
    // other -- correctness needs every column in the key, not just the
    // `ORDER BY` ones, since a column absent from the key could differ
    // between two rows without ever breaking their adjacency. Which
    // columns come first only affects output order, not which rows
    // land next to which.
    let sort_key = if is_distinct {
        let mut keys = sort_key.unwrap_or_default();
        keys.extend((0..output_count).map(|idx| OrderByPlan {
            target: OrderByTarget::Column(idx),
            descending: false,
            nulls_first: false,
        }));
        Some(keys)
    } else {
        sort_key
    };

    let mut em = Emitter::new();
    let mut reg = RegAlloc::new();

    // db-core#182: a program must open its own cursors. `from_subquery`
    // materializes onto `cursor` itself (`Opcode::OpenEphemeral`, below)
    // so it must not also be opened as a real table here.
    if from_subquery.is_none() {
        em.emit(Instruction::new(
            Opcode::OpenRead,
            cursor,
            super::valid_table_root_page(schema)?,
            0,
        ));
    }
    if let Some((right_schema, right_cursor)) = right {
        em.emit(Instruction::new(
            Opcode::OpenRead,
            right_cursor,
            super::valid_table_root_page(right_schema)?,
            0,
        ));
    }

    // The sorter cursor uses a slot past every cursor the caller wired
    // up -- `Opcode::SorterOpen` opens it itself at runtime, so it needs
    // no caller-side wiring, just an id that can't collide. The index
    // cursor the #94 fast paths below open takes the slot after it.
    let sorter_cursor = right
        .map_or(cursor, |(_, c)| cursor.max(c))
        .saturating_add(1);
    let index_cursor = sorter_cursor.saturating_add(1);
    // Every cursor above was picked by arithmetic rather than through
    // `RegAlloc`, so the subquery cursors #95 allocates must start past
    // them.
    reg.reserve_cursors_through(index_cursor);

    // db-core#95: `FROM (SELECT ...) alias` -- run the subquery into an
    // ephemeral table on `cursor` first, so everything below scans it
    // exactly like a real table.
    if let Some(subquery) = from_subquery {
        super::subquery::materialize_from_subquery(&mut em, &mut reg, subquery, catalog, cursor)?;
    }

    let limit = limit_scan::compile_limit_setup(&mut em, &mut reg, &scope, query)?;

    // An index-ordered scan produces the requested order straight out of
    // the b-tree, so it replaces the sorter entirely rather than feeding
    // it -- hence it is tried before `SorterOpen` is ever emitted.
    if right.is_none() && !has_projected_expr {
        let index_end_label = em.new_label();
        if index_scan::try_compile_index_ordered_scan(
            &mut em,
            &mut reg,
            query,
            &scope,
            &columns,
            cursor,
            index_cursor,
            limit,
            index_end_label,
        )? {
            em.place(index_end_label);
            em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
            return Ok(em.finish());
        }
    }

    // The sort key's final indices aren't known until the scan body
    // below actually compiles each `OrderByTarget::Expr` into a
    // register (its record position depends on how many registers the
    // expression itself allocates), so `SorterOpen` is emitted with a
    // placeholder `P4` here and patched once that body resolves the
    // real `Vec<SortKeyColumn>` (db-core#167).
    let sorter_open_addr = sort_key.is_some().then(|| {
        em.emit(Instruction::with_p4(
            Opcode::SorterOpen,
            sorter_cursor,
            0,
            0,
            P4::SortKey(Vec::new()),
        ))
    });

    let end_label = em.new_label();

    // A `WHERE`-bounded indexed column seeks straight to its first
    // matching index entry instead of scanning every row and filtering;
    // it still feeds the sorter above when there's an `ORDER BY`, so
    // unlike the index-ordered scan it slots in where the sequential
    // scan would go.
    let seeked = right.is_none()
        && !has_projected_expr
        && range_scan::try_compile_range_seek(
            &mut em,
            &mut reg,
            query,
            &scope,
            &columns,
            sort_key.clone(),
            sorter_open_addr,
            sorter_cursor,
            cursor,
            index_cursor,
            limit,
            end_label,
        )?;
    if seeked {
        em.place(end_label);
        return finish_scan(
            em,
            reg,
            sort_key.is_some(),
            sorter_cursor,
            output_count,
            limit,
            end_label,
            is_distinct,
        );
    }

    let outer_rewind_addr = em.emit(Instruction::new(Opcode::Rewind, cursor, 0, 0));
    em.patch_p2(outer_rewind_addr, end_label);

    let outer_loop_start = em.new_label();
    em.place(outer_loop_start);
    let outer_row_skip = em.new_label();

    // `FULL OUTER`'s limit-guard target must skip *both* passes, not
    // just this outer scan -- `end_label` here is where pass two (if
    // any) begins, not the true program end, so it can't double as the
    // limit target the way it does for every other join kind (where
    // there is no second pass, and the two labels are the same point).
    let final_label = if matches!(join.map(|j| j.op), Some(JoinOp::Full)) {
        em.new_label()
    } else {
        end_label
    };

    match (join, right) {
        (Some(join), Some((_, right_cursor))) => {
            compile_join_body(
                &mut em,
                &mut reg,
                &scope,
                join,
                right_cursor,
                query,
                &columns,
                sort_key.clone(),
                sorter_open_addr,
                sorter_cursor,
                limit,
                final_label,
                outer_row_skip,
            )?;
        }
        _ => {
            if let Some(where_expr) = &query.where_clause {
                super::compile_cond(
                    &mut em,
                    &mut reg,
                    &scope,
                    where_expr,
                    // `WHERE` is where SQL's three-valued logic collapses
                    // to two: an unknown predicate excludes the row
                    // exactly like a false one.
                    CondTargets::null_is_false(Target::Fallthrough, Target::Jump(outer_row_skip)),
                )?;
            }
            emit_row(
                &mut em,
                &mut reg,
                &scope,
                &columns,
                None,
                sort_key.clone(),
                sorter_open_addr,
                sorter_cursor,
                limit,
                outer_row_skip,
                end_label,
            )?;
        }
    }

    em.place(outer_row_skip);
    let next_addr = em.emit(Instruction::new(Opcode::Next, cursor, 0, 0));
    em.patch_p2(next_addr, outer_loop_start);

    em.place(end_label);

    if let (Some(join), Some((_, right_cursor))) = (join, right) {
        if join.op == JoinOp::Full {
            compile_full_outer_right_pass(
                &mut em,
                &mut reg,
                &scope,
                join,
                cursor,
                right_cursor,
                &columns,
                sort_key.clone(),
                sorter_open_addr,
                sorter_cursor,
                limit,
                final_label,
            )?;
        }
    }

    finish_scan(
        em,
        reg,
        sort_key.is_some(),
        sorter_cursor,
        output_count,
        limit,
        end_label,
        is_distinct,
    )
}

/// Emits a scan's shared tail: the post-sort drain loop (when there's an
/// `ORDER BY`, where `LIMIT`/`OFFSET` apply to the *sorted* output
/// rather than scan order) and the terminating `Halt`. Shared by the
/// ordinary sequential scan and #94's range-seek fast path, which differ
/// only in how they reach it.
#[allow(clippy::too_many_arguments)]
fn finish_scan(
    mut em: Emitter,
    mut reg: RegAlloc,
    has_sort_key: bool,
    sorter_cursor: i32,
    output_count: usize,
    limit: Option<LimitState>,
    end_label: Label,
    distinct: bool,
) -> Result<Program> {
    // `end_label` is placed by the caller, not here: `FULL OUTER`'s
    // second pass deliberately begins at it, so it can't be rebound to
    // the drain loop's address.
    let _ = end_label;

    if has_sort_key {
        let sorter_end_label = em.new_label();
        let sort_addr = em.emit(Instruction::new(Opcode::SorterSort, sorter_cursor, 0, 0));
        em.patch_p2(sort_addr, sorter_end_label);

        // `DISTINCT` (db-core#176): the sort key already covers every
        // output column (see `compile_select_inner`'s own `is_distinct`
        // handling), so two identical rows always land adjacent in the
        // drain -- tracking the last *emitted* row and comparing each
        // new one against it, mirroring `aggregate::emit_boundary_check`,
        // is enough to collapse the run. The comparison happens before
        // `OFFSET`/`LIMIT` are consulted below: a duplicate must not
        // consume either budget, only a genuinely distinct row may.
        let dedup = distinct.then(|| {
            let zero_reg = reg.alloc();
            em.emit(Instruction::new(Opcode::Integer, 0, zero_reg, 0));
            let have_prev_reg = reg.alloc();
            em.emit(Instruction::new(Opcode::Integer, 0, have_prev_reg, 0));
            let prev_regs: Vec<i32> = (0..output_count).map(|_| reg.alloc()).collect();
            (zero_reg, have_prev_reg, prev_regs)
        });

        let sorter_loop_start = em.new_label();
        em.place(sorter_loop_start);
        let sorter_row_skip = em.new_label();

        let mut regs = Vec::with_capacity(output_count);
        for idx in 0..output_count {
            let r = reg.alloc();
            em.emit(Instruction::new(
                Opcode::Column,
                sorter_cursor,
                i32::try_from(idx).map_err(|_| CodegenError::Unsupported {
                    reason: format!("column index {idx} does not fit in a p2 operand"),
                })?,
                r,
            ));
            regs.push(r);
        }

        if let Some((zero_reg, have_prev_reg, prev_regs)) = &dedup {
            let boundary_label = em.new_label();
            let first_row_check =
                em.emit(Instruction::new(Opcode::Eq, *have_prev_reg, 0, *zero_reg));
            em.patch_p2(first_row_check, boundary_label);
            for (&cur, &prev) in regs.iter().zip(prev_regs) {
                let a_null = em.new_label();
                let same_col = em.new_label();
                let a_null_addr = em.emit(Instruction::new(Opcode::IsNull, cur, 0, 0));
                em.patch_p2(a_null_addr, a_null);
                let b_null_addr = em.emit(Instruction::new(Opcode::IsNull, prev, 0, 0));
                em.patch_p2(b_null_addr, boundary_label);
                let eq_addr = em.emit(Instruction::new(Opcode::Eq, cur, 0, prev));
                em.patch_p2(eq_addr, same_col);
                let goto_boundary = em.emit(Instruction::new(Opcode::Goto, 0, 0, 0));
                em.patch_p2(goto_boundary, boundary_label);
                em.place(a_null);
                let b_not_null_addr = em.emit(Instruction::new(Opcode::NotNull, prev, 0, 0));
                em.patch_p2(b_not_null_addr, boundary_label);
                em.place(same_col);
            }
            // Every column matched the previous row: a duplicate, which
            // must not reach `ResultRow` or consume `OFFSET`/`LIMIT`.
            let goto_skip = em.emit(Instruction::new(Opcode::Goto, 0, 0, 0));
            em.patch_p2(goto_skip, sorter_row_skip);

            em.place(boundary_label);
            for (&cur, &prev) in regs.iter().zip(prev_regs) {
                em.emit(Instruction::new(Opcode::Copy, cur, prev, 0));
            }
            em.emit(Instruction::new(Opcode::Integer, 1, *have_prev_reg, 0));
        }

        if let Some(limit) = &limit {
            limit_scan::emit_offset_guard(&mut em, limit, sorter_row_skip);
            limit_scan::emit_limit_guard(&mut em, limit, sorter_end_label);
        }

        if let Some(&first) = regs.first() {
            em.emit(Instruction::new(
                Opcode::ResultRow,
                first,
                i32::try_from(output_count).map_err(|_| CodegenError::Unsupported {
                    reason: format!(
                        "SELECT list of {output_count} columns does not fit in a p2 operand"
                    ),
                })?,
                0,
            ));
        } else {
            em.emit(Instruction::new(Opcode::ResultRow, reg.alloc(), 0, 0));
        }

        em.place(sorter_row_skip);
        let sorter_next_addr = em.emit(Instruction::new(Opcode::SorterNext, sorter_cursor, 0, 0));
        em.patch_p2(sorter_next_addr, sorter_loop_start);

        em.place(sorter_end_label);
    }

    em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));

    Ok(em.finish())
}

/// Compiles a `SELECT` that aggregates -- an explicit `GROUP BY`, or a
/// whole-table aggregate with no `GROUP BY` key at all -- through
/// [`super::aggregate`] (db-core#93).
///
/// `ORDER BY` and `DISTINCT` combined with aggregation are rejected
/// rather than composed, matching sqlite-rs's own documented
/// simplification for this slice.
fn compile_aggregate_select(
    schema: &TableSchema,
    cursor: i32,
    right: Option<(&TableSchema, i32)>,
    query: &Select,
    scope: &Scope,
) -> Result<Program> {
    if !query.order_by.is_empty() {
        return Err(CodegenError::Unsupported {
            reason: "ORDER BY combined with GROUP BY/aggregation is not yet supported".to_string(),
        });
    }
    if query.limit.as_ref().is_some_and(|l| l.offset.is_some()) {
        return Err(CodegenError::Unsupported {
            reason: "OFFSET combined with GROUP BY/aggregation is not yet supported".to_string(),
        });
    }

    let mut em = Emitter::new();
    let mut reg = RegAlloc::new();

    // db-core#182: a program must open its own cursors rather than
    // relying on a caller to pre-wire them.
    em.emit(Instruction::new(
        Opcode::OpenRead,
        cursor,
        super::valid_table_root_page(schema)?,
        0,
    ));
    if let Some((right_schema, right_cursor)) = right {
        em.emit(Instruction::new(
            Opcode::OpenRead,
            right_cursor,
            super::valid_table_root_page(right_schema)?,
            0,
        ));
    }

    let limit_reg = compile_limit_setup(&mut em, &mut reg, scope, query)?;
    let end_label = em.new_label();

    let highest = right.map_or(cursor, |(_, c)| cursor.max(c));
    let cursors = super::aggregate::ScanCursors::past(cursor, highest);
    let mut sink = |em: &mut Emitter, reg: &mut RegAlloc, first: i32, count: usize| -> Result<()> {
        emit_result_row(em, reg, first, count)
    };

    match right {
        Some((_, right_cursor)) => super::aggregate::compile_joined_grouped_scan(
            &mut em,
            &mut reg,
            query,
            scope,
            cursors,
            right_cursor,
            limit_reg,
            end_label,
            &mut sink,
        )?,
        None => super::aggregate::compile_aggregate_scan(
            &mut em, &mut reg, query, schema, cursors, limit_reg, end_label, &mut sink,
        )?,
    }

    em.place(end_label);
    em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
    Ok(em.finish())
}

/// Emits the `LIMIT` counter register, if any -- `Opcode::IfNotZero`
/// decrements it per emitted row (see [`emit_limit_guard`]).
fn compile_limit_setup(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    scope: &Scope,
    query: &Select,
) -> Result<Option<i32>> {
    let Some(limit) = &query.limit else {
        return Ok(None);
    };
    Ok(Some(super::value::compile_value(
        em,
        reg,
        scope,
        &limit.limit,
    )?))
}

fn emit_result_row(em: &mut Emitter, reg: &mut RegAlloc, first: i32, count: usize) -> Result<()> {
    if count == 0 {
        em.emit(Instruction::new(Opcode::ResultRow, reg.alloc(), 0, 0));
        return Ok(());
    }
    em.emit(Instruction::new(
        Opcode::ResultRow,
        first,
        i32::try_from(count).map_err(|_| CodegenError::Unsupported {
            reason: format!("SELECT list of {count} columns does not fit in a p2 operand"),
        })?,
        0,
    ));
    Ok(())
}

/// Strips an optional `table.` qualifier off `name`.
fn unqualified(name: &str) -> &str {
    name.split_once('.').map_or(name, |(_, rest)| rest)
}

/// The condition `join` matches on, as an [`Expr`].
///
/// `expr::Join` carried `left_col`/`right_col` -- an equi-join was the
/// only shape it could represent, so this function *synthesized* the
/// comparison. The AST carries the real `ON <expr>` instead (#147), so
/// an arbitrary join condition now flows through untouched and the
/// synthesis is needed only for `USING`, which names columns rather
/// than writing the comparison out.
///
/// `USING (c, ...)` qualifies both sides explicitly: the columns belong
/// to a specific table structurally, unlike a bare column reference
/// elsewhere in the query, so leaving them unqualified would send the
/// right side to the left table via `Scope`'s
/// unqualified-defaults-to-left convention.
pub(super) fn build_join_cond(scope: &Scope, join: &Join) -> Result<Expr> {
    if join.natural {
        return Err(CodegenError::Unsupported {
            reason: "NATURAL JOIN is not supported yet".to_string(),
        });
    }
    match &join.constraint {
        Some(JoinConstraint::On(expr)) => Ok(expr.clone()),
        Some(JoinConstraint::Using(cols)) => {
            let right_table_name = scope
                .right
                .as_ref()
                .map(|(right_schema, _)| right_schema.name.clone())
                .unwrap_or_default();
            let mut conds = cols.iter().map(|col| {
                let col = unqualified(col);
                Expr {
                    kind: ExprKind::Binary {
                        op: BinaryOp::Eq,
                        lhs: Box::new(super::column_expr(format!("{}.{col}", scope.schema.name))),
                        rhs: Box::new(super::column_expr(format!("{right_table_name}.{col}"))),
                    },
                    span: crate::parser::Span::UNKNOWN,
                }
            });
            // `USING` requires at least one column (the grammar
            // enforces it), so `next()` is `Some` for any parsed join.
            let Some(first) = conds.next() else {
                return Err(CodegenError::Unsupported {
                    reason: "USING with no columns".to_string(),
                });
            };
            Ok(conds.fold(first, |acc, cond| Expr {
                kind: ExprKind::Binary {
                    op: BinaryOp::And,
                    lhs: Box::new(acc),
                    rhs: Box::new(cond),
                },
                span: crate::parser::Span::UNKNOWN,
            }))
        }
        None => Err(CodegenError::Unsupported {
            reason: "a JOIN without ON or USING is not supported".to_string(),
        }),
    }
}

/// Compiles the inner-join loop for `join` against `right_cursor`,
/// nested inside the already-open outer scan. Handles `LEFT`'s
/// null-extension: a `matched` flag tracks whether the `ON` condition
/// (not `WHERE`) matched any inner row, and a null-extended row is
/// emitted once, after the inner loop, when it never did.
#[allow(clippy::too_many_arguments)]
fn compile_join_body(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    scope: &Scope,
    join: &Join,
    right_cursor: i32,
    query: &Select,
    columns: &[ProjectedColumn],
    sort_key: Option<Vec<OrderByPlan>>,
    sorter_open_addr: Option<usize>,
    sorter_cursor: i32,
    limit: Option<LimitState>,
    end_label: Label,
    outer_row_skip: Label,
) -> Result<()> {
    // `LEFT` and `FULL` both null-extend an outer row that never matched
    // any inner row; `FULL` additionally needs a second pass (see
    // `compile_full_outer_right_pass`) for right rows no outer row ever
    // matched.
    let matched_reg = if matches!(join.op, JoinOp::Left | JoinOp::Full) {
        let r = reg.alloc();
        em.emit(Instruction::new(Opcode::Integer, 0, r, 0));
        Some(r)
    } else {
        None
    };

    let inner_end_label = em.new_label();
    let inner_rewind_addr = em.emit(Instruction::new(Opcode::Rewind, right_cursor, 0, 0));
    em.patch_p2(inner_rewind_addr, inner_end_label);

    let inner_loop_start = em.new_label();
    em.place(inner_loop_start);
    let inner_row_skip = em.new_label();

    let join_cond = build_join_cond(scope, join)?;
    super::compile_cond(
        em,
        reg,
        scope,
        &join_cond,
        CondTargets::null_is_false(Target::Fallthrough, Target::Jump(inner_row_skip)),
    )?;

    if let Some(m) = matched_reg {
        em.emit(Instruction::new(Opcode::Integer, 1, m, 0));
    }

    if let Some(where_expr) = &query.where_clause {
        super::compile_cond(
            em,
            reg,
            scope,
            where_expr,
            CondTargets::null_is_false(Target::Fallthrough, Target::Jump(inner_row_skip)),
        )?;
    }

    emit_row(
        em,
        reg,
        scope,
        columns,
        None,
        sort_key.clone(),
        sorter_open_addr,
        sorter_cursor,
        limit,
        inner_row_skip,
        end_label,
    )?;

    em.place(inner_row_skip);
    let inner_next_addr = em.emit(Instruction::new(Opcode::Next, right_cursor, 0, 0));
    em.patch_p2(inner_next_addr, inner_loop_start);
    em.place(inner_end_label);

    if let Some(m) = matched_reg {
        let zero_reg = reg.alloc();
        em.emit(Instruction::new(Opcode::Integer, 0, zero_reg, 0));
        let eq_addr = em.emit(Instruction::new(Opcode::Eq, m, 0, zero_reg));
        let null_ext_label = em.new_label();
        em.patch_p2(eq_addr, null_ext_label);
        em.goto(outer_row_skip);

        em.place(null_ext_label);
        emit_row(
            em,
            reg,
            scope,
            columns,
            Some(right_cursor),
            sort_key,
            sorter_open_addr,
            sorter_cursor,
            limit,
            outer_row_skip,
            end_label,
        )?;
    }

    Ok(())
}

/// `FULL OUTER`'s second pass (db-core#101): right-outer, left-inner,
/// emitting a right-null-extended row for every right row the first
/// pass's inner loop never matched. Matching here re-checks only the
/// `ON` condition (not `WHERE`) against every left row -- mirroring
/// `compile_join_body`'s own simplification of skipping `WHERE` on its
/// null-extension path -- so a right row already emitted (matched, or
/// filtered out by `WHERE`) in the first pass is never emitted twice:
/// once any left row satisfies `ON`, the first pass already produced
/// this right row's output (or explicitly filtered it), and this pass
/// only fires when no left row satisfies `ON` at all.
#[allow(clippy::too_many_arguments)]
fn compile_full_outer_right_pass(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    scope: &Scope,
    join: &Join,
    left_cursor: i32,
    right_cursor: i32,
    columns: &[ProjectedColumn],
    sort_key: Option<Vec<OrderByPlan>>,
    sorter_open_addr: Option<usize>,
    sorter_cursor: i32,
    limit: Option<LimitState>,
    final_label: Label,
) -> Result<()> {
    let right_rewind_addr = em.emit(Instruction::new(Opcode::Rewind, right_cursor, 0, 0));
    em.patch_p2(right_rewind_addr, final_label);

    let pass_loop_start = em.new_label();
    em.place(pass_loop_start);
    let pass_row_skip = em.new_label();

    let matched_reg = reg.alloc();
    em.emit(Instruction::new(Opcode::Integer, 0, matched_reg, 0));

    let left_end_label = em.new_label();
    let left_rewind_addr = em.emit(Instruction::new(Opcode::Rewind, left_cursor, 0, 0));
    em.patch_p2(left_rewind_addr, left_end_label);

    let left_loop_start = em.new_label();
    em.place(left_loop_start);
    let left_row_skip = em.new_label();

    let join_cond = build_join_cond(scope, join)?;
    super::compile_cond(
        em,
        reg,
        scope,
        &join_cond,
        CondTargets::null_is_false(Target::Fallthrough, Target::Jump(left_row_skip)),
    )?;
    em.emit(Instruction::new(Opcode::Integer, 1, matched_reg, 0));

    em.place(left_row_skip);
    let left_next_addr = em.emit(Instruction::new(Opcode::Next, left_cursor, 0, 0));
    em.patch_p2(left_next_addr, left_loop_start);
    em.place(left_end_label);

    let zero_reg = reg.alloc();
    em.emit(Instruction::new(Opcode::Integer, 0, zero_reg, 0));
    let eq_addr = em.emit(Instruction::new(Opcode::Eq, matched_reg, 0, zero_reg));
    let unmatched_label = em.new_label();
    em.patch_p2(eq_addr, unmatched_label);
    em.goto(pass_row_skip);

    em.place(unmatched_label);
    emit_row(
        em,
        reg,
        scope,
        columns,
        Some(left_cursor),
        sort_key,
        sorter_open_addr,
        sorter_cursor,
        limit,
        pass_row_skip,
        final_label,
    )?;

    em.place(pass_row_skip);
    let pass_next_addr = em.emit(Instruction::new(Opcode::Next, right_cursor, 0, 0));
    em.patch_p2(pass_next_addr, pass_loop_start);
    em.place(final_label);

    Ok(())
}

/// Rewrites every column reference in `expr` that resolves to
/// `null_cursor` into a `NULL` literal, so a compiled `ORDER BY`
/// expression evaluates exactly as it would if that cursor's row were
/// truly null-extended -- matching `compile_row_values`'s existing
/// null-fill for a plain column, which `compile_value` alone has no
/// way to apply (db-core#173). A nested subquery's own body is left
/// untouched: a correlated reference to the null-extended side inside
/// it is a separate, harder problem this narrow fix doesn't take on.
fn null_extend(scope: &Scope, null_cursor: i32, expr: &Expr) -> Expr {
    let kind = match &expr.kind {
        ExprKind::Column { table, name, .. } => {
            match scope.resolve(&qualified_name(table.as_deref(), name)) {
                Ok((cursor, _)) if cursor == null_cursor => ExprKind::Literal(Literal::Null),
                _ => expr.kind.clone(),
            }
        }
        ExprKind::Literal(_)
        | ExprKind::Param(_)
        | ExprKind::Subquery(_)
        | ExprKind::Exists { .. }
        | ExprKind::InSubquery { .. }
        | ExprKind::InSubqueryMulti { .. } => expr.kind.clone(),
        ExprKind::FunctionCall {
            name,
            distinct,
            args,
            over,
        } => ExprKind::FunctionCall {
            name: name.clone(),
            distinct: *distinct,
            args: match args {
                FunctionArgs::Star => FunctionArgs::Star,
                FunctionArgs::List(list) => FunctionArgs::List(
                    list.iter()
                        .map(|e| null_extend(scope, null_cursor, e))
                        .collect(),
                ),
            },
            over: over.clone(),
        },
        ExprKind::Unary { op, expr: inner } => ExprKind::Unary {
            op: *op,
            expr: Box::new(null_extend(scope, null_cursor, inner)),
        },
        ExprKind::Binary { op, lhs, rhs } => ExprKind::Binary {
            op: *op,
            lhs: Box::new(null_extend(scope, null_cursor, lhs)),
            rhs: Box::new(null_extend(scope, null_cursor, rhs)),
        },
        ExprKind::Is { lhs, rhs, negated } => ExprKind::Is {
            lhs: Box::new(null_extend(scope, null_cursor, lhs)),
            rhs: Box::new(null_extend(scope, null_cursor, rhs)),
            negated: *negated,
        },
        ExprKind::IsNull {
            expr: inner,
            negated,
        } => ExprKind::IsNull {
            expr: Box::new(null_extend(scope, null_cursor, inner)),
            negated: *negated,
        },
        ExprKind::Between {
            expr: inner,
            lo,
            hi,
            negated,
        } => ExprKind::Between {
            expr: Box::new(null_extend(scope, null_cursor, inner)),
            lo: Box::new(null_extend(scope, null_cursor, lo)),
            hi: Box::new(null_extend(scope, null_cursor, hi)),
            negated: *negated,
        },
        ExprKind::In {
            expr: inner,
            list,
            negated,
        } => ExprKind::In {
            expr: Box::new(null_extend(scope, null_cursor, inner)),
            list: list
                .iter()
                .map(|e| null_extend(scope, null_cursor, e))
                .collect(),
            negated: *negated,
        },
        ExprKind::Like {
            expr: inner,
            pattern,
            glob,
            negated,
            escape,
        } => ExprKind::Like {
            expr: Box::new(null_extend(scope, null_cursor, inner)),
            pattern: Box::new(null_extend(scope, null_cursor, pattern)),
            glob: *glob,
            negated: *negated,
            escape: escape
                .as_ref()
                .map(|e| Box::new(null_extend(scope, null_cursor, e))),
        },
        ExprKind::Case {
            operand,
            whens,
            else_,
        } => ExprKind::Case {
            operand: operand
                .as_ref()
                .map(|e| Box::new(null_extend(scope, null_cursor, e))),
            whens: whens
                .iter()
                .map(|(cond, result)| {
                    (
                        null_extend(scope, null_cursor, cond),
                        null_extend(scope, null_cursor, result),
                    )
                })
                .collect(),
            else_: else_
                .as_ref()
                .map(|e| Box::new(null_extend(scope, null_cursor, e))),
        },
        ExprKind::Cast {
            expr: inner,
            type_name,
        } => ExprKind::Cast {
            expr: Box::new(null_extend(scope, null_cursor, inner)),
            type_name: type_name.clone(),
        },
        ExprKind::Collate {
            expr: inner,
            collation,
        } => ExprKind::Collate {
            expr: Box::new(null_extend(scope, null_cursor, inner)),
            collation: collation.clone(),
        },
        ExprKind::Paren(inner) => ExprKind::Paren(Box::new(null_extend(scope, null_cursor, inner))),
    };
    Expr {
        kind,
        span: expr.span,
    }
}

/// Emits one output row: either directly via `ResultRow` (no `ORDER BY`,
/// applying the `LIMIT` guard first), or into the sorter via
/// `MakeRecord`/`SorterInsert` (an `ORDER BY` is present, so `LIMIT`
/// applies later, during the post-sort drain). `null_cursor`, when set,
/// null-fills every column that would otherwise be read from it -- used
/// for `LEFT` join's unmatched-row null-extension. `row_skip` is where
/// an `OFFSET`-skipped row continues -- the caller's own per-row skip
/// label, so the skip lands on the loop's advance instruction.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_row(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    scope: &Scope,
    columns: &[ProjectedColumn],
    null_cursor: Option<i32>,
    sort_key: Option<Vec<OrderByPlan>>,
    sorter_open_addr: Option<usize>,
    sorter_cursor: i32,
    limit: Option<LimitState>,
    row_skip: Label,
    end_label: Label,
) -> Result<()> {
    if let Some(plans) = sort_key {
        let (first, count) = compile_row_values(em, reg, scope, columns, null_cursor)?;
        // Every `OrderByTarget::Expr` compiles into its own register,
        // appended after the row's own columns; its record position is
        // that register's offset from `first`, resolved only now since
        // it depends on how many registers the expression itself
        // allocates. A `Column` target's index is already fixed at
        // plan time -- it's a position within `columns` above.
        let mut sort_keys = Vec::with_capacity(plans.len());
        for plan in &plans {
            let index = match &plan.target {
                OrderByTarget::Column(idx) => *idx,
                OrderByTarget::Expr(expr) => {
                    let expr = match null_cursor {
                        Some(null_cursor) => null_extend(scope, null_cursor, expr),
                        None => expr.clone(),
                    };
                    let r = compile_value(em, reg, scope, &expr)?;
                    usize::try_from(r.saturating_sub(first)).unwrap_or(0)
                }
            };
            sort_keys.push(SortKeyColumn {
                index,
                descending: plan.descending,
                collation: Collation::Binary,
                nulls_first: plan.nulls_first,
            });
        }
        if let Some(addr) = sorter_open_addr {
            em.patch_p4(addr, P4::SortKey(sort_keys));
        }
        // Widen the record to cover any expression registers appended
        // past the original `count` columns -- `reg`'s watermark is the
        // authoritative span since an expression's own final register
        // need not be its highest allocated one (e.g. `CASE` allocates
        // its destination before its branches).
        let count = usize::try_from(reg.peek().saturating_sub(first)).unwrap_or(count);
        let blob_reg = reg.alloc();
        em.emit(Instruction::new(
            Opcode::MakeRecord,
            first,
            i32::try_from(count).map_err(|_| CodegenError::Unsupported {
                reason: format!("SELECT list of {count} columns does not fit in a p2 operand"),
            })?,
            blob_reg,
        ));
        em.emit(Instruction::new(
            Opcode::SorterInsert,
            sorter_cursor,
            blob_reg,
            0,
        ));
        return Ok(());
    }

    if let Some(limit) = &limit {
        limit_scan::emit_offset_guard(em, limit, row_skip);
        limit_scan::emit_limit_guard(em, limit, end_label);
    }
    let (first, count) = compile_row_values(em, reg, scope, columns, null_cursor)?;
    em.emit(Instruction::new(
        Opcode::ResultRow,
        first,
        i32::try_from(count).map_err(|_| CodegenError::Unsupported {
            reason: format!("SELECT list of {count} columns does not fit in a p2 operand"),
        })?,
        0,
    ));
    Ok(())
}

/// Emits the `LIMIT` stop-guard: called once per row, before emitting
/// it. `IfNotZero` decrements `limit_reg` only while it's positive and
/// jumps whenever it's nonzero -- a negative `LIMIT` (the "no limit"
/// convention) never reaches zero and always takes that jump, staying
/// unbounded -- so the `Goto` below is reached only when `limit_reg`
/// has hit exactly zero, stopping the scan before this row is emitted.
/// Checking before emitting (not after) matters for `LIMIT 0`: an
/// after-the-fact check would let the first row escape before the
/// guard ever ran.
pub(super) fn emit_limit_guard(em: &mut Emitter, limit_reg: i32, end_label: super::Label) {
    let has_budget_addr = em.emit(Instruction::new(Opcode::IfNotZero, limit_reg, 0, 0));
    let stop_addr = em.emit(Instruction::new(Opcode::Goto, 0, 0, 0));
    em.patch_p2(stop_addr, end_label);
    let continue_label = em.new_label();
    em.patch_p2(has_budget_addr, continue_label);
    em.place(continue_label);
}

/// Compiles each of `names` into a register, returning the contiguous
/// `(first_register, count)` window `Opcode::ResultRow` reads. A
/// [`ProjectedColumn::Name`] lands in a freshly allocated, and therefore
/// already-adjacent, register by construction; a
/// [`ProjectedColumn::Expr`] compiles through [`compile_value`] instead
/// (db-core#168), which may allocate more than one register internally
/// -- the contiguity check/`Copy`-based fallback below handles both
/// cases uniformly rather than assuming registers always land adjacent.
///
/// `null_cursor`, when set, null-fills every column resolved to that
/// cursor instead of reading it -- `LEFT` join's unmatched-row
/// null-extension, where the right cursor holds no current row to read.
fn compile_row_values(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    scope: &Scope,
    names: &[ProjectedColumn],
    null_cursor: Option<i32>,
) -> Result<(i32, usize)> {
    let mut regs = Vec::with_capacity(names.len());
    for column in names {
        let r = match column {
            ProjectedColumn::Name(name) => {
                let (cursor, idx) = scope.resolve(name)?;
                let r = reg.alloc();
                if Some(cursor) == null_cursor {
                    em.emit(Instruction::new(Opcode::Null, 0, r, 0));
                } else {
                    let table_schema = match &scope.right {
                        Some((right_schema, right_cursor)) if cursor == *right_cursor => {
                            right_schema
                        }
                        _ => &scope.schema,
                    };
                    emit_column_read(em, table_schema, cursor, idx, r)?;
                }
                r
            }
            ProjectedColumn::Expr(expr) => {
                let expr = match null_cursor {
                    Some(null_cursor) => null_extend(scope, null_cursor, expr),
                    None => expr.clone(),
                };
                compile_value(em, reg, scope, &expr)?
            }
        };
        regs.push(r);
    }
    // An empty SELECT list still needs one register for `ResultRow` to name.
    let Some(&first) = regs.first() else {
        return Ok((reg.alloc(), 0));
    };
    let already_contiguous = regs
        .iter()
        .enumerate()
        .all(|(i, &r)| r == first.saturating_add(i32::try_from(i).unwrap_or(i32::MAX)));
    if already_contiguous {
        return Ok((first, regs.len()));
    }
    // `Copy` never allocates, so alloc-then-emit per column still yields
    // one contiguous block of destination registers.
    let first_dest = reg.alloc();
    em.emit(Instruction::new(Opcode::Copy, first, first_dest, 0));
    for &r in regs.iter().skip(1) {
        let dest = reg.alloc();
        em.emit(Instruction::new(Opcode::Copy, r, dest, 0));
    }
    Ok((first_dest, regs.len()))
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
    use crate::codegen::row::testutil::select as query;
    use crate::vm::row::{execute, Cursor, InMemoryCursor, InMemoryIndexCursor, Value, Vm};

    fn schema(columns: &[&str]) -> TableSchema {
        schema_named("t", columns)
    }

    fn schema_named(name: &str, columns: &[&str]) -> TableSchema {
        TableSchema {
            name: name.into(),
            columns: columns.iter().map(|c| (*c).to_string()).collect(),
            column_types: columns.iter().map(|_| String::new()).collect(),
            rowid_alias: None,
            root_page: 0,
            indexes: vec![],
        }
    }

    fn run(schema: &TableSchema, query: &Select, rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
        let program = compile_select(schema, 0, query).unwrap();
        let mut vm = Vm::new();
        vm.open_cursor(0, Box::new(InMemoryCursor::new(rows)))
            .unwrap();
        execute(&mut vm, &program).unwrap()
    }

    fn run_join(
        schema: &TableSchema,
        right_schema: &TableSchema,
        query: &Select,
        left_rows: Vec<Vec<Value>>,
        right_rows: Vec<Vec<Value>>,
    ) -> Vec<Vec<Value>> {
        let program = compile_select_join(schema, 0, right_schema, 1, query).unwrap();
        let mut vm = Vm::new();
        vm.open_cursor(0, Box::new(InMemoryCursor::new(left_rows)))
            .unwrap();
        vm.open_cursor(1, Box::new(InMemoryCursor::new(right_rows)))
            .unwrap();
        execute(&mut vm, &program).unwrap()
    }

    #[test]
    fn scans_every_row_projecting_selected_columns() {
        let schema = schema(&["a", "b"]);
        let query = query("SELECT b FROM t");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1), Value::Integer(10)],
                vec![Value::Integer(2), Value::Integer(20)],
            ],
        );
        assert_eq!(
            rows,
            vec![vec![Value::Integer(10)], vec![Value::Integer(20)]]
        );
    }

    #[test]
    fn distinct_collapses_duplicate_single_column_rows() {
        let schema = schema(&["a"]);
        let query = query("SELECT DISTINCT a FROM t ORDER BY a");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
                vec![Value::Integer(1)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]);
    }

    #[test]
    fn distinct_dedups_the_whole_row_not_per_column() {
        let schema = schema(&["a", "b"]);
        let query = query("SELECT DISTINCT a, b FROM t ORDER BY a, b");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1), Value::Integer(1)],
                vec![Value::Integer(1), Value::Integer(2)],
                vec![Value::Integer(1), Value::Integer(1)],
            ],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1), Value::Integer(1)],
                vec![Value::Integer(1), Value::Integer(2)],
            ]
        );
    }

    #[test]
    fn distinct_composes_with_limit_counting_only_distinct_rows() {
        let schema = schema(&["a"]);
        let query = query("SELECT DISTINCT a FROM t ORDER BY a LIMIT 1");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(1)]]);
    }

    #[test]
    fn distinct_treats_two_nulls_as_equal() {
        let schema = schema(&["a"]);
        let query = query("SELECT DISTINCT a FROM t");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Null],
                vec![Value::Null],
                vec![Value::Integer(1)],
            ],
        );
        assert_eq!(rows.len(), 2, "{rows:?}");
    }

    #[test]
    fn select_list_arithmetic_expression_compiles_and_executes() {
        let schema = schema(&["a"]);
        let query = query("SELECT a + 1 FROM t");
        let rows = run(
            &schema,
            &query,
            vec![vec![Value::Integer(1)], vec![Value::Integer(2)]],
        );
        assert_eq!(rows, vec![vec![Value::Integer(2)], vec![Value::Integer(3)]]);
    }

    #[test]
    fn select_list_function_call_compiles_and_executes() {
        let schema = schema(&["name"]);
        let query = query("SELECT upper(name) FROM t");
        let rows = run(
            &schema,
            &query,
            vec![vec![Value::Text("abc".to_string().into())]],
        );
        assert_eq!(rows, vec![vec![Value::Text("ABC".to_string().into())]]);
    }

    #[test]
    fn select_list_mixes_bare_column_and_expression() {
        let schema = schema(&["a", "b"]);
        let query = query("SELECT a, b + 1 FROM t");
        let rows = run(
            &schema,
            &query,
            vec![vec![Value::Integer(1), Value::Integer(10)]],
        );
        assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(11)]]);
    }

    #[test]
    fn select_list_expression_composes_with_order_by_on_a_bare_column() {
        let schema = schema(&["a"]);
        let query = query("SELECT a + 1 FROM t ORDER BY a DESC");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(3)],
                vec![Value::Integer(2)],
            ],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(4)],
                vec![Value::Integer(3)],
                vec![Value::Integer(2)],
            ]
        );
    }

    #[test]
    fn table_star_on_a_single_table_matches_bare_star() {
        let schema = schema(&["a", "b"]);
        let query = query("SELECT t.* FROM t");
        let rows = run(
            &schema,
            &query,
            vec![vec![Value::Integer(1), Value::Integer(10)]],
        );
        assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(10)]]);
    }

    #[test]
    fn table_star_restricts_to_one_side_of_a_join() {
        let left = schema_named("a", &["x"]);
        let right = schema_named("b", &["y"]);
        let query = query("SELECT a.*, b.* FROM a JOIN b ON a.x = b.y");
        let rows = run_join(
            &left,
            &right,
            &query,
            vec![vec![Value::Integer(1)]],
            vec![vec![Value::Integer(1)]],
        );
        assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(1)]]);
    }

    #[test]
    fn table_star_over_an_unknown_table_is_unsupported() {
        let schema = schema(&["a"]);
        let err = compile_select(&schema, 0, &query("SELECT bogus.* FROM t")).unwrap_err();
        match err {
            CodegenError::Unsupported { reason } => {
                assert!(reason.contains("unknown table"), "{reason}");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn select_list_scalar_subquery_compiles_and_executes() {
        // The literal example from #163's own acceptance criteria,
        // previously blocked by this ticket's bare-column-only
        // restriction: a scalar subquery directly in the SELECT list.
        let outer_schema = schema_named("t", &["a"]);
        let mut inner_schema = schema_named("s", &["x"]);
        inner_schema.root_page = 3;
        let catalog = vec![outer_schema.clone(), inner_schema];
        let sql = "SELECT (SELECT x FROM s) FROM t";
        let ast_query = query(sql);
        let program = compile_select_with_catalog(&catalog, &ast_query).unwrap();

        let mut vm = Vm::new();
        vm.open_cursor(
            0,
            Box::new(InMemoryCursor::new(vec![vec![Value::Integer(1)]])),
        )
        .unwrap();
        let sub_slot = match program
            .instructions
            .iter()
            .find(|i| i.opcode == Opcode::OpenRead && i.p1 != 0)
        {
            Some(instr) => instr.p1,
            None => panic!("compiled program opens a subquery cursor"),
        };
        vm.open_cursor(
            sub_slot,
            Box::new(InMemoryCursor::new(vec![vec![Value::Integer(42)]])),
        )
        .unwrap();
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(42)]]);
    }

    #[test]
    fn star_expands_to_every_schema_column_in_order() {
        let schema = schema(&["a", "b"]);
        let query = query("SELECT * FROM t");
        let rows = run(
            &schema,
            &query,
            vec![vec![Value::Integer(1), Value::Integer(10)]],
        );
        assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(10)]]);
    }

    #[test]
    fn where_clause_filters_rows() {
        let schema = schema(&["a"]);
        let query = query("SELECT a FROM t WHERE a > 1");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
                vec![Value::Integer(3)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(2)], vec![Value::Integer(3)]]);
    }

    #[test]
    fn limit_stops_the_scan_early() {
        let schema = schema(&["a"]);
        let query = query("SELECT a FROM t LIMIT 2");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
                vec![Value::Integer(3)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]);
    }

    #[test]
    fn limit_zero_emits_no_rows() {
        let schema = schema(&["a"]);
        let query = query("SELECT a FROM t LIMIT 0");
        let rows = run(&schema, &query, vec![vec![Value::Integer(1)]]);
        assert!(rows.is_empty());
    }

    fn indexed_schema(columns: &[&str], index_column: &str) -> TableSchema {
        let mut schema = schema(columns);
        schema.column_types = columns.iter().map(|_| "INTEGER".to_string()).collect();
        schema.root_page = 2;
        schema.indexes = vec![super::super::IndexSchema {
            name: format!("t_{index_column}"),
            root_page: 3,
            columns: vec![index_column.to_string()],
        }];
        schema
    }

    /// Runs `query` with an index cursor pre-wired on the slot #94's fast
    /// paths open (`sorter_cursor + 1`), built from the same rows the
    /// table cursor holds.
    fn run_indexed(
        schema: &TableSchema,
        query: &Select,
        rows: Vec<Vec<Value>>,
        index_column: usize,
    ) -> Vec<Vec<Value>> {
        let program = compile_select(schema, 0, query).unwrap();
        let mut vm = Vm::new();
        let mut index = InMemoryIndexCursor::new(vec![SortKeyColumn {
            index: index_column,
            descending: false,
            collation: Collation::Binary,
            nulls_first: false,
        }]);
        for (i, row) in rows.iter().enumerate() {
            index.insert(i32::try_from(i).unwrap() as i64 + 1, row.clone());
        }
        vm.open_cursor(0, Box::new(InMemoryCursor::new(rows)))
            .unwrap();
        vm.open_cursor(2, Box::new(index)).unwrap();
        execute(&mut vm, &program).unwrap()
    }

    fn opcodes(schema: &TableSchema, query: &Select) -> Vec<Opcode> {
        compile_select(schema, 0, query)
            .unwrap()
            .instructions
            .iter()
            .map(|i| i.opcode)
            .collect()
    }

    /// SQLite's grammar has no bare `OFFSET` -- only `LIMIT n OFFSET m`
    /// -- so a very large `LIMIT` stands in for "no real limit" when a
    /// test only cares about the offset. `expr::Query` had them as
    /// independent `Option<usize>` fields, so the old suite could build
    /// a query the parser could never actually produce.
    #[test]
    fn offset_skips_leading_rows() {
        let schema = schema(&["a"]);
        let query = query("SELECT a FROM t LIMIT 1000000 OFFSET 2");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
                vec![Value::Integer(3)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(3)]]);
    }

    #[test]
    fn limit_applies_after_offset() {
        let schema = schema(&["a"]);
        let query = query("SELECT a FROM t LIMIT 2 OFFSET 1");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
                vec![Value::Integer(3)],
                vec![Value::Integer(4)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(2)], vec![Value::Integer(3)]]);
    }

    #[test]
    fn offset_applies_to_sorted_output_not_scan_order() {
        let schema = schema(&["a"]);
        let query = query("SELECT a FROM t ORDER BY a LIMIT 1000000 OFFSET 1");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(3)],
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(2)], vec![Value::Integer(3)]]);
    }

    #[test]
    fn an_indexed_order_by_walks_the_index_instead_of_sorting() {
        let schema = indexed_schema(&["a"], "a");
        let query = query("SELECT a FROM t ORDER BY a");
        let ops = opcodes(&schema, &query);
        assert!(ops.contains(&Opcode::IdxRewind));
        assert!(ops.contains(&Opcode::IdxRowid));
        assert!(!ops.contains(&Opcode::SorterOpen));

        let rows = run_indexed(
            &schema,
            &query,
            vec![
                vec![Value::Integer(3)],
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
            ],
            0,
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
                vec![Value::Integer(3)]
            ]
        );
    }

    #[test]
    fn a_descending_indexed_order_by_walks_the_index_backward() {
        let schema = indexed_schema(&["a"], "a");
        let query = query("SELECT a FROM t ORDER BY a DESC");
        let ops = opcodes(&schema, &query);
        assert!(ops.contains(&Opcode::IdxLast));
        assert!(ops.contains(&Opcode::IdxPrev));

        let rows = run_indexed(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(3)],
                vec![Value::Integer(2)],
            ],
            0,
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(3)],
                vec![Value::Integer(2)],
                vec![Value::Integer(1)]
            ]
        );
    }

    #[test]
    fn an_index_ordered_scan_still_honours_limit_and_offset() {
        let schema = indexed_schema(&["a"], "a");
        let query = query("SELECT a FROM t ORDER BY a LIMIT 1 OFFSET 1");
        let rows = run_indexed(
            &schema,
            &query,
            vec![
                vec![Value::Integer(3)],
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
            ],
            0,
        );
        assert_eq!(rows, vec![vec![Value::Integer(2)]]);
    }

    #[test]
    fn an_unindexed_order_by_still_uses_the_sorter() {
        let schema = indexed_schema(&["a", "b"], "a");
        let query = query("SELECT b FROM t ORDER BY b");
        let ops = opcodes(&schema, &query);
        assert!(ops.contains(&Opcode::SorterOpen));
        assert!(!ops.contains(&Opcode::IdxRewind));
    }

    #[test]
    fn an_inclusive_lower_bound_seeks_the_index() {
        let schema = indexed_schema(&["a"], "a");
        let query = query("SELECT a FROM t WHERE a >= 2");
        let ops = opcodes(&schema, &query);
        assert!(ops.contains(&Opcode::SeekIndexGE));
        assert!(!ops.contains(&Opcode::Rewind));

        let rows = run_indexed(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
                vec![Value::Integer(3)],
            ],
            0,
        );
        assert_eq!(rows, vec![vec![Value::Integer(2)], vec![Value::Integer(3)]]);
    }

    #[test]
    fn an_exclusive_lower_bound_skips_the_equal_run() {
        let schema = indexed_schema(&["a"], "a");
        let query = query("SELECT a FROM t WHERE a > 1");
        let rows = run_indexed(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
            ],
            0,
        );
        assert_eq!(rows, vec![vec![Value::Integer(2)]]);
    }

    #[test]
    fn a_two_sided_range_stops_at_the_upper_bound() {
        let schema = indexed_schema(&["a"], "a");
        let query = query("SELECT a FROM t WHERE a >= 2 AND a <= 3");
        let ops = opcodes(&schema, &query);
        assert!(ops.contains(&Opcode::IdxCompareGT));

        let rows = run_indexed(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
                vec![Value::Integer(3)],
                vec![Value::Integer(4)],
            ],
            0,
        );
        assert_eq!(rows, vec![vec![Value::Integer(2)], vec![Value::Integer(3)]]);
    }

    #[test]
    fn an_equality_on_an_indexed_column_seeks_that_key_only() {
        let schema = indexed_schema(&["a"], "a");
        let query = query("SELECT a FROM t WHERE a = 2");
        let rows = run_indexed(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
                vec![Value::Integer(2)],
                vec![Value::Integer(3)],
            ],
            0,
        );
        assert_eq!(rows, vec![vec![Value::Integer(2)], vec![Value::Integer(2)]]);
    }

    #[test]
    fn an_unindexed_where_clause_falls_back_to_the_sequential_scan() {
        let schema = indexed_schema(&["a", "b"], "a");
        let query = query("SELECT b FROM t WHERE b > 1");
        let ops = opcodes(&schema, &query);
        assert!(ops.contains(&Opcode::Rewind));
        assert!(!ops.contains(&Opcode::SeekIndexGE));
    }

    #[test]
    fn a_join_never_takes_an_index_fast_path() {
        let schema = indexed_schema(&["a"], "a");
        let mut right = indexed_schema(&["x"], "x");
        right.name = "u".into();
        let query = query("SELECT a FROM t JOIN u ON t.a = u.x WHERE a >= 1");
        let program = compile_select_join(&schema, 0, &right, 1, &query).unwrap();
        let ops: Vec<Opcode> = program.instructions.iter().map(|i| i.opcode).collect();
        assert!(!ops.contains(&Opcode::SeekIndexGE));
    }

    #[test]
    fn empty_table_scans_zero_rows() {
        let schema = schema(&["a"]);
        let query = query("SELECT a FROM t");
        let rows = run(&schema, &query, vec![]);
        assert!(rows.is_empty());
    }

    #[test]
    fn join_without_a_right_cursor_is_unsupported() {
        let schema = schema(&["a"]);
        let query = query("SELECT a FROM t JOIN u ON t.a = u.b");
        assert!(matches!(
            compile_select(&schema, 0, &query),
            Err(CodegenError::Unsupported { .. })
        ));
    }

    #[test]
    fn right_join_is_unsupported() {
        let schema = schema(&["a"]);
        let right = schema_named("u", &["b"]);
        let query = query("SELECT a FROM t RIGHT JOIN u ON t.a = u.b");
        assert!(matches!(
            compile_select_join(&schema, 0, &right, 1, &query),
            Err(CodegenError::Unsupported { .. })
        ));
    }

    #[test]
    fn full_outer_join_null_extends_both_unmatched_sides() {
        let left = schema(&["a"]);
        let right = schema_named("u", &["b", "c"]);
        let query = query("SELECT a, u.c FROM t FULL JOIN u ON t.a = u.b");
        let rows = run_join(
            &left,
            &right,
            &query,
            vec![vec![Value::Integer(1)], vec![Value::Integer(2)]],
            vec![
                vec![Value::Integer(1), Value::Integer(100)],
                vec![Value::Integer(3), Value::Integer(300)],
            ],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1), Value::Integer(100)],
                vec![Value::Integer(2), Value::Null],
                vec![Value::Null, Value::Integer(300)],
            ]
        );
    }

    #[test]
    fn full_outer_join_with_no_matches_null_extends_every_row() {
        let left = schema(&["a"]);
        let right = schema_named("u", &["b", "c"]);
        let query = query("SELECT a, u.c FROM t FULL JOIN u ON t.a = u.b");
        let rows = run_join(
            &left,
            &right,
            &query,
            vec![vec![Value::Integer(1)]],
            vec![vec![Value::Integer(2), Value::Integer(200)]],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1), Value::Null],
                vec![Value::Null, Value::Integer(200)],
            ]
        );
    }

    #[test]
    fn full_outer_join_with_empty_left_null_extends_every_right_row() {
        let left = schema(&["a"]);
        let right = schema_named("u", &["b", "c"]);
        let query = query("SELECT a, u.c FROM t FULL JOIN u ON t.a = u.b");
        let rows = run_join(
            &left,
            &right,
            &query,
            vec![],
            vec![
                vec![Value::Integer(1), Value::Integer(100)],
                vec![Value::Integer(2), Value::Integer(200)],
            ],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Null, Value::Integer(100)],
                vec![Value::Null, Value::Integer(200)],
            ]
        );
    }

    #[test]
    fn inner_join_matches_rows_on_equi_condition() {
        let left = schema(&["a"]);
        let right = schema_named("u", &["b", "c"]);
        let query = query("SELECT a, u.c FROM t JOIN u ON t.a = u.b");
        let rows = run_join(
            &left,
            &right,
            &query,
            vec![vec![Value::Integer(1)], vec![Value::Integer(2)]],
            vec![
                vec![Value::Integer(1), Value::Integer(100)],
                vec![Value::Integer(3), Value::Integer(300)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(100)]]);
    }

    #[test]
    fn left_join_null_extends_unmatched_rows() {
        let left = schema(&["a"]);
        let right = schema_named("u", &["b", "c"]);
        let query = query("SELECT a, u.c FROM t LEFT JOIN u ON t.a = u.b");
        let rows = run_join(
            &left,
            &right,
            &query,
            vec![vec![Value::Integer(1)], vec![Value::Integer(2)]],
            vec![vec![Value::Integer(1), Value::Integer(100)]],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1), Value::Integer(100)],
                vec![Value::Integer(2), Value::Null],
            ]
        );
    }

    #[test]
    fn order_by_sorts_rows() {
        let schema = schema(&["a"]);
        let query = query("SELECT a FROM t ORDER BY a");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(3)],
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
            ],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
                vec![Value::Integer(3)],
            ]
        );
    }

    #[test]
    fn order_by_descending_sorts_rows() {
        let schema = schema(&["a"]);
        let query = query("SELECT a FROM t ORDER BY a DESC");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(3)],
                vec![Value::Integer(2)],
            ],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(3)],
                vec![Value::Integer(2)],
                vec![Value::Integer(1)],
            ]
        );
    }

    #[test]
    fn order_by_column_absent_from_select_list_still_sorts() {
        let schema = schema(&["a", "b"]);
        let query = query("SELECT b FROM t ORDER BY a");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(2), Value::Integer(20)],
                vec![Value::Integer(1), Value::Integer(10)],
            ],
        );
        assert_eq!(
            rows,
            vec![vec![Value::Integer(10)], vec![Value::Integer(20)]]
        );
    }

    #[test]
    fn multi_term_order_by_sorts_by_every_term_in_order() {
        let schema = schema(&["a", "b"]);
        let query = query("SELECT a, b FROM t ORDER BY a, b DESC");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1), Value::Integer(1)],
                vec![Value::Integer(2), Value::Integer(2)],
                vec![Value::Integer(1), Value::Integer(2)],
                vec![Value::Integer(2), Value::Integer(1)],
            ],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1), Value::Integer(2)],
                vec![Value::Integer(1), Value::Integer(1)],
                vec![Value::Integer(2), Value::Integer(2)],
                vec![Value::Integer(2), Value::Integer(1)],
            ]
        );
    }

    #[test]
    fn order_by_over_an_arithmetic_expression_sorts_by_its_computed_value() {
        let schema = schema(&["a", "b"]);
        let query = query("SELECT a, b FROM t ORDER BY a + b");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1), Value::Integer(5)], // sum 6
                vec![Value::Integer(2), Value::Integer(1)], // sum 3
                vec![Value::Integer(3), Value::Integer(0)], // sum 3
            ],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(2), Value::Integer(1)],
                vec![Value::Integer(3), Value::Integer(0)],
                vec![Value::Integer(1), Value::Integer(5)],
            ]
        );
    }

    #[test]
    fn order_by_over_a_function_call_sorts_by_its_computed_value() {
        let schema = schema(&["name"]);
        let query = query("SELECT name FROM t ORDER BY upper(name)");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Text("banana".to_string().into())],
                vec![Value::Text("Apple".to_string().into())],
                vec![Value::Text("cherry".to_string().into())],
            ],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Text("Apple".to_string().into())],
                vec![Value::Text("banana".to_string().into())],
                vec![Value::Text("cherry".to_string().into())],
            ]
        );
    }

    #[test]
    fn multi_term_order_by_composes_a_bare_column_and_an_expression() {
        let schema = schema(&["a", "b"]);
        let query = query("SELECT a, b FROM t ORDER BY a, b + 1 DESC");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1), Value::Integer(1)],
                vec![Value::Integer(1), Value::Integer(2)],
                vec![Value::Integer(2), Value::Integer(5)],
            ],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1), Value::Integer(2)],
                vec![Value::Integer(1), Value::Integer(1)],
                vec![Value::Integer(2), Value::Integer(5)],
            ]
        );
    }

    #[test]
    fn order_by_nulls_last_sorts_nulls_after_values_ascending() {
        let schema = schema(&["a"]);
        let query = query("SELECT a FROM t ORDER BY a NULLS LAST");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(2)],
                vec![Value::Null],
                vec![Value::Integer(1)],
            ],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
                vec![Value::Null],
            ]
        );
    }

    #[test]
    fn order_by_nulls_first_sorts_nulls_before_values_ascending() {
        let schema = schema(&["a"]);
        let query = query("SELECT a FROM t ORDER BY a NULLS FIRST");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(2)],
                vec![Value::Null],
                vec![Value::Integer(1)],
            ],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Null],
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
            ]
        );
    }

    #[test]
    fn order_by_desc_defaults_to_nulls_first() {
        let schema = schema(&["a"]);
        let query = query("SELECT a FROM t ORDER BY a DESC");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Null],
                vec![Value::Integer(2)],
            ],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Null],
                vec![Value::Integer(2)],
                vec![Value::Integer(1)],
            ]
        );
    }

    #[test]
    fn expression_limit_and_offset_are_computed_at_runtime() {
        let schema = schema(&["a"]);
        let query = query("SELECT a FROM t ORDER BY a LIMIT 1 + 1 OFFSET 3 - 2");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
                vec![Value::Integer(3)],
                vec![Value::Integer(4)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(2)], vec![Value::Integer(3)]]);
    }

    #[test]
    fn order_by_respects_limit_on_sorted_output() {
        let schema = schema(&["a"]);
        let query = query("SELECT a FROM t ORDER BY a LIMIT 2");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(3)],
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]);
    }

    #[test]
    fn full_outer_join_order_by_sorts_both_passes_together() {
        let left = schema(&["a"]);
        let right = schema_named("u", &["b", "c"]);
        let query = query("SELECT a, u.c FROM t FULL JOIN u ON t.a = u.b ORDER BY a");
        let rows = run_join(
            &left,
            &right,
            &query,
            vec![vec![Value::Integer(2)]],
            vec![
                vec![Value::Integer(1), Value::Integer(100)],
                vec![Value::Integer(2), Value::Integer(200)],
            ],
        );
        // `a` is NULL for the right-unmatched row (b=1), so it sorts
        // last under this sorter's `nulls_first: false` default.
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(2), Value::Integer(200)],
                vec![Value::Null, Value::Integer(100)],
            ]
        );
    }

    #[test]
    fn full_outer_join_order_by_expression_null_extends_the_left_side() {
        let left = schema(&["a"]);
        let right = schema_named("u", &["b", "c"]);
        let query = query("SELECT a, u.c FROM t FULL JOIN u ON t.a = u.b ORDER BY a + 0");
        let rows = run_join(
            &left,
            &right,
            &query,
            vec![vec![Value::Integer(5)], vec![Value::Integer(1)]],
            vec![
                vec![Value::Integer(5), Value::Integer(500)],
                vec![Value::Integer(9), Value::Integer(900)],
            ],
        );
        // `b=9` matches no left row, so its `a` is null-extended in the
        // second pass -- `a + 0` must evaluate against that NULL, not
        // whatever real value the left cursor's last-visited row (a=1)
        // still holds in its registers (db-core#173). If it read the
        // stale value instead, this row's sort key would tie with the
        // real `a=1` row instead of sorting last, and it would show up
        // out of order below.
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1), Value::Null],
                vec![Value::Integer(5), Value::Integer(500)],
                vec![Value::Null, Value::Integer(900)],
            ]
        );
    }

    #[test]
    fn full_outer_join_limit_applies_across_both_passes() {
        let left = schema(&["a"]);
        let right = schema_named("u", &["b", "c"]);
        let query = query("SELECT a, u.c FROM t FULL JOIN u ON t.a = u.b LIMIT 1");
        let rows = run_join(
            &left,
            &right,
            &query,
            vec![vec![Value::Integer(1)]],
            vec![
                vec![Value::Integer(1), Value::Integer(100)],
                vec![Value::Integer(2), Value::Integer(200)],
            ],
        );
        // The first pass alone hits LIMIT 1, so the second (right-outer)
        // pass never runs -- the unmatched right row is never emitted.
        assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(100)]]);
    }

    #[test]
    fn window_select_item_is_unsupported() {
        let schema = schema(&["a"]);
        let query = query("SELECT ROW_NUMBER() OVER (ORDER BY a) FROM t");
        assert!(matches!(
            compile_select(&schema, 0, &query),
            Err(CodegenError::Unsupported { .. })
        ));
    }

    // db-core#93: `GROUP BY`/`HAVING`/aggregation, ported alongside
    // `codegen::row::aggregate` from sqlite-rs's own codegen tests for
    // that slice.

    #[test]
    fn whole_table_count_star_over_an_empty_table_still_emits_one_row() {
        let schema = schema(&["a"]);
        let query = query("SELECT COUNT(*) FROM t");
        let rows = run(&schema, &query, vec![]);
        assert_eq!(rows, vec![vec![Value::Integer(0)]]);
    }

    #[test]
    fn whole_table_aggregates_over_an_empty_table_finalize_to_null() {
        let schema = schema(&["a"]);
        let query = query("SELECT SUM(a), MIN(a), MAX(a), AVG(a) FROM t");
        let rows = run(&schema, &query, vec![]);
        assert_eq!(
            rows,
            vec![vec![Value::Null, Value::Null, Value::Null, Value::Null]]
        );
    }

    #[test]
    fn whole_table_aggregates_fold_every_row() {
        let schema = schema(&["a"]);
        let query = query("SELECT COUNT(*), SUM(a), MIN(a), MAX(a), AVG(a) FROM t");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(5)],
                vec![Value::Integer(3)],
            ],
        );
        assert_eq!(
            rows,
            vec![vec![
                Value::Integer(3),
                Value::Integer(9),
                Value::Integer(1),
                Value::Integer(5),
                Value::Real(3.0),
            ]]
        );
    }

    #[test]
    fn whole_table_aggregate_honours_the_where_clause() {
        let schema = schema(&["a"]);
        let query = query("SELECT COUNT(*) FROM t WHERE a > 1");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
                vec![Value::Integer(3)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(2)]]);
    }

    #[test]
    fn group_by_emits_one_row_per_group_in_key_order() {
        let schema = schema(&["g", "v"]);
        let query = query("SELECT g, COUNT(*), SUM(v) FROM t GROUP BY g");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(2), Value::Integer(10)],
                vec![Value::Integer(1), Value::Integer(1)],
                vec![Value::Integer(2), Value::Integer(20)],
                vec![Value::Integer(1), Value::Integer(2)],
            ],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1), Value::Integer(2), Value::Integer(3)],
                vec![Value::Integer(2), Value::Integer(2), Value::Integer(30)],
            ]
        );
    }

    #[test]
    fn group_by_over_an_arithmetic_expression_groups_by_its_computed_value() {
        // The SELECT-list itself stays bare-column/aggregate-only here
        // (a computed, non-aggregate result column is a separate,
        // still-open limitation, db-core#175) -- this only exercises
        // `GROUP BY`'s own key, via `COUNT(*)` per group.
        let schema = schema(&["a", "b"]);
        let query = query("SELECT COUNT(*) FROM t GROUP BY a + b");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1), Value::Integer(1)],
                vec![Value::Integer(0), Value::Integer(2)],
                vec![Value::Integer(3), Value::Integer(4)],
            ],
        );
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows.contains(&vec![Value::Integer(1)]), "{rows:?}");
        assert!(rows.contains(&vec![Value::Integer(2)]), "{rows:?}");
    }

    #[test]
    fn group_by_expression_composes_with_having() {
        let schema = schema(&["a", "b"]);
        let query = query("SELECT COUNT(*) FROM t GROUP BY a + b HAVING \"COUNT(*)\" > 1");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1), Value::Integer(1)],
                vec![Value::Integer(0), Value::Integer(2)],
                vec![Value::Integer(3), Value::Integer(4)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(2)]]);
    }

    #[test]
    fn group_by_over_an_empty_table_emits_no_rows() {
        let schema = schema(&["g", "v"]);
        let query = query("SELECT g, COUNT(*) FROM t GROUP BY g");
        assert!(run(&schema, &query, vec![]).is_empty());
    }

    #[test]
    fn group_by_collects_null_keys_into_one_group() {
        let schema = schema(&["g", "v"]);
        let query = query("SELECT g, COUNT(*) FROM t GROUP BY g");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Null, Value::Integer(1)],
                vec![Value::Integer(1), Value::Integer(2)],
                vec![Value::Null, Value::Integer(3)],
            ],
        );
        assert!(rows.contains(&vec![Value::Null, Value::Integer(2)]));
        assert!(rows.contains(&vec![Value::Integer(1), Value::Integer(1)]));
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn group_by_two_keys_groups_on_the_pair() {
        let schema = schema(&["a", "b", "v"]);
        let query = query("SELECT a, b, SUM(v) FROM t GROUP BY a, b");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1), Value::Integer(1), Value::Integer(10)],
                vec![Value::Integer(1), Value::Integer(2), Value::Integer(20)],
                vec![Value::Integer(1), Value::Integer(1), Value::Integer(5)],
            ],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1), Value::Integer(1), Value::Integer(15)],
                vec![Value::Integer(1), Value::Integer(2), Value::Integer(20)],
            ]
        );
    }

    #[test]
    fn having_filters_whole_groups_after_aggregation() {
        let schema = schema(&["g", "v"]);
        let query = query("SELECT g, COUNT(*) FROM t GROUP BY g HAVING \"COUNT(*)\" > 1");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1), Value::Integer(10)],
                vec![Value::Integer(2), Value::Integer(20)],
                vec![Value::Integer(2), Value::Integer(30)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(2), Value::Integer(2)]]);
    }

    /// A `HAVING` aggregate absent from the `SELECT` list still gets its
    /// own accumulator slot -- sqlite-rs's `collect_aggregates` scans the
    /// `HAVING` clause too.
    #[test]
    fn having_may_reference_an_aggregate_absent_from_the_select_list() {
        let schema = schema(&["g", "v"]);
        let query = query("SELECT g FROM t GROUP BY g HAVING \"SUM(v)\" >= 50");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1), Value::Integer(10)],
                vec![Value::Integer(2), Value::Integer(20)],
                vec![Value::Integer(2), Value::Integer(30)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(2)]]);
    }

    /// A `HAVING` sharing its aggregate with a result column accumulates
    /// into a single slot, so both read the same finalized value.
    #[test]
    fn having_shares_one_slot_with_an_identical_result_column() {
        let schema = schema(&["g", "v"]);
        let query = query("SELECT g, SUM(v) FROM t GROUP BY g HAVING \"SUM(v)\" > 15");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1), Value::Integer(10)],
                vec![Value::Integer(2), Value::Integer(20)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(2), Value::Integer(20)]]);
    }

    #[test]
    fn having_on_a_whole_table_aggregate_may_suppress_the_only_row() {
        let schema = schema(&["a"]);
        let query = query("SELECT COUNT(*) FROM t HAVING \"COUNT(*)\" > 5");
        let rows = run(&schema, &query, vec![vec![Value::Integer(1)]]);
        assert!(rows.is_empty());
    }

    #[test]
    fn limit_applies_to_groups_not_to_scanned_rows() {
        let schema = schema(&["g", "v"]);
        let query = query("SELECT g, COUNT(*) FROM t GROUP BY g LIMIT 1");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1), Value::Integer(10)],
                vec![Value::Integer(1), Value::Integer(11)],
                vec![Value::Integer(2), Value::Integer(20)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(2)]]);
    }

    /// A plain (non-aggregate) column that isn't a `GROUP BY` key takes
    /// an "arbitrary row" from the group -- the group's first row, which
    /// is what both strategies retain.
    #[test]
    fn a_non_key_plain_column_reads_the_groups_first_row() {
        let schema = schema(&["g", "v"]);
        let query = query("SELECT v, COUNT(*) FROM t GROUP BY g");
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1), Value::Integer(10)],
                vec![Value::Integer(1), Value::Integer(11)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(10), Value::Integer(2)]]);
    }

    #[test]
    fn star_alongside_an_aggregate_expands_to_every_schema_column() {
        let schema = schema(&["g", "v"]);
        let query = query("SELECT *, COUNT(*) FROM t GROUP BY g");
        let rows = run(
            &schema,
            &query,
            vec![vec![Value::Integer(1), Value::Integer(10)]],
        );
        assert_eq!(
            rows,
            vec![vec![
                Value::Integer(1),
                Value::Integer(10),
                Value::Integer(1)
            ]]
        );
    }

    #[test]
    fn aggregate_over_an_inner_join_folds_only_matched_rows() {
        let left = schema(&["a"]);
        let right = schema_named("u", &["b", "c"]);
        let query = query("SELECT COUNT(*), SUM(u.c) FROM t JOIN u ON t.a = u.b");
        let rows = run_join(
            &left,
            &right,
            &query,
            vec![vec![Value::Integer(1)], vec![Value::Integer(2)]],
            vec![
                vec![Value::Integer(1), Value::Integer(100)],
                vec![Value::Integer(3), Value::Integer(300)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(100)]]);
    }

    #[test]
    fn group_by_over_an_inner_join_groups_on_a_left_column() {
        let left = schema(&["a"]);
        let right = schema_named("u", &["b", "c"]);
        let query = query("SELECT a, SUM(u.c) FROM t JOIN u ON t.a = u.b GROUP BY a");
        let rows = run_join(
            &left,
            &right,
            &query,
            vec![vec![Value::Integer(1)], vec![Value::Integer(2)]],
            vec![
                vec![Value::Integer(1), Value::Integer(100)],
                vec![Value::Integer(1), Value::Integer(50)],
                vec![Value::Integer(2), Value::Integer(20)],
            ],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1), Value::Integer(150)],
                vec![Value::Integer(2), Value::Integer(20)],
            ]
        );
    }

    /// `LEFT`'s null-extended row still forms a group: `COUNT(*)` counts
    /// it, but `SUM` over the null-extended right column skips it.
    #[test]
    fn group_by_over_a_left_join_keeps_unmatched_outer_rows() {
        let left = schema(&["a"]);
        let right = schema_named("u", &["b", "c"]);
        let query =
            query("SELECT a, COUNT(*), SUM(u.c) FROM t LEFT JOIN u ON t.a = u.b GROUP BY a");
        let rows = run_join(
            &left,
            &right,
            &query,
            vec![vec![Value::Integer(1)], vec![Value::Integer(2)]],
            vec![vec![Value::Integer(1), Value::Integer(100)]],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1), Value::Integer(1), Value::Integer(100)],
                vec![Value::Integer(2), Value::Integer(1), Value::Null],
            ]
        );
    }

    #[test]
    fn having_over_an_inner_join_filters_groups_by_their_aggregate() {
        let left = schema(&["a"]);
        let right = schema_named("u", &["b", "c"]);
        let query = query(
            "SELECT a, COUNT(*) FROM t JOIN u ON t.a = u.b GROUP BY a HAVING \"COUNT(*)\" > 1",
        );
        let rows = run_join(
            &left,
            &right,
            &query,
            vec![vec![Value::Integer(1)], vec![Value::Integer(2)]],
            vec![
                vec![Value::Integer(1), Value::Integer(100)],
                vec![Value::Integer(1), Value::Integer(50)],
                vec![Value::Integer(2), Value::Integer(20)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(2)]]);
    }

    #[test]
    fn having_over_a_join_may_reference_either_sides_column() {
        let left = schema(&["a"]);
        let right = schema_named("u", &["b", "c"]);
        let query =
            query("SELECT a, COUNT(*) FROM t JOIN u ON t.a = u.b GROUP BY a HAVING u.c > 30");
        let rows = run_join(
            &left,
            &right,
            &query,
            vec![vec![Value::Integer(1)], vec![Value::Integer(2)]],
            vec![
                vec![Value::Integer(1), Value::Integer(100)],
                vec![Value::Integer(2), Value::Integer(20)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(1)]]);
    }

    #[test]
    fn full_outer_join_combined_with_aggregation_is_unsupported() {
        let left = schema(&["a"]);
        let right = schema_named("u", &["b"]);
        let query = query("SELECT COUNT(*) FROM t FULL JOIN u ON t.a = u.b");
        assert!(matches!(
            compile_select_join(&left, 0, &right, 1, &query),
            Err(CodegenError::Unsupported { .. })
        ));
    }

    #[test]
    fn order_by_combined_with_aggregation_is_unsupported() {
        let schema = schema(&["g"]);
        let query = query("SELECT g, COUNT(*) FROM t GROUP BY g ORDER BY g");
        assert!(matches!(
            compile_select(&schema, 0, &query),
            Err(CodegenError::Unsupported { .. })
        ));
    }

    #[test]
    fn group_by_an_unknown_column_is_rejected() {
        let schema = schema(&["a"]);
        let query = query("SELECT COUNT(*) FROM t GROUP BY nope");
        assert!(matches!(
            compile_select(&schema, 0, &query),
            Err(CodegenError::UnknownColumn(_))
        ));
    }

    /// The sort-then-group strategy is the fallback the hash strategy
    /// declines to; exercised directly here so both strategies are
    /// covered on an explicit `GROUP BY`, not just via the join path.
    #[test]
    fn the_sort_strategy_produces_the_same_groups_as_the_hash_strategy() {
        let schema = schema(&["g", "v"]);
        let query = query("SELECT g, SUM(v) FROM t GROUP BY g");
        let rows = vec![
            vec![Value::Integer(2), Value::Integer(10)],
            vec![Value::Integer(1), Value::Integer(1)],
            vec![Value::Integer(2), Value::Integer(20)],
        ];

        let mut em = Emitter::new();
        let mut reg = RegAlloc::new();
        let end_label = em.new_label();
        let cursors = super::super::aggregate::ScanCursors::past(0, 0);
        super::super::aggregate::compile_grouped_scan(
            &mut em,
            &mut reg,
            &query,
            &schema,
            cursors,
            None,
            end_label,
            false,
            &mut |em: &mut Emitter, reg: &mut RegAlloc, first: i32, count: usize| {
                emit_result_row(em, reg, first, count)
            },
        )
        .unwrap();
        em.place(end_label);
        em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
        let program = em.finish();

        let mut vm = Vm::new();
        vm.open_cursor(0, Box::new(InMemoryCursor::new(rows.clone())))
            .unwrap();
        let sorted = execute(&mut vm, &program).unwrap();
        assert_eq!(sorted, run(&schema, &query, rows));
        assert_eq!(
            sorted,
            vec![
                vec![Value::Integer(1), Value::Integer(1)],
                vec![Value::Integer(2), Value::Integer(30)],
            ]
        );
    }

    /// MC/DC vector (obligation `select_210`, `compile_select_inner`'s
    /// aggregate-dispatch decision `!group_by.is_empty() ||
    /// query_has_aggregate`): both leaves false -- a plain projection
    /// stays on the row-scan path and emits no aggregate opcodes.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__select_210__v1_no_group_by_no_aggregate_scans_rows() {
        let schema = schema(&["a"]);
        let ops = opcodes(&schema, &query("SELECT a FROM t"));
        assert!(!ops.contains(&Opcode::AggStep));
        assert!(!ops.contains(&Opcode::OpenPseudo));
    }

    /// MC/DC vector (obligation `select_210`): leaf A (`GROUP BY`
    /// present) true with no aggregate function -- flips the outcome
    /// against `mcdc__select_210__v1_no_group_by_no_aggregate_scans_rows`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__select_210__v2_group_by_alone_takes_aggregate_path() {
        let schema = schema(&["a"]);
        let ops = opcodes(&schema, &query("SELECT a FROM t GROUP BY a"));
        // A `GROUP BY` without an aggregate function has no `AggStep`,
        // but the aggregate path alone reads its group rows back through
        // an `OpenPseudo` cursor -- the row-scan path never emits one.
        assert!(ops.contains(&Opcode::OpenPseudo));
    }

    /// MC/DC vector (obligation `select_210`): leaf B (an aggregate
    /// function) true with no `GROUP BY` -- flips the outcome against
    /// `mcdc__select_210__v1_no_group_by_no_aggregate_scans_rows`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__select_210__v3_aggregate_alone_takes_aggregate_path() {
        let schema = schema(&["a"]);
        let ops = opcodes(&schema, &query("SELECT COUNT(*) FROM t"));
        assert!(ops.contains(&Opcode::AggStep));
        assert!(ops.contains(&Opcode::AggFinal));
    }

    /// MC/DC vector (obligation `select_419`, the index-ordered-scan
    /// gate `right.is_none() && !has_projected_expr`): both leaves true
    /// -- a single-table, bare-column `ORDER BY` on an indexed column
    /// walks the index and never opens a sorter.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__select_419__v1_single_table_bare_columns_walks_index() {
        let schema = indexed_schema(&["a"], "a");
        let ops = opcodes(&schema, &query("SELECT a FROM t ORDER BY a"));
        assert!(ops.contains(&Opcode::IdxRewind));
        assert!(!ops.contains(&Opcode::SorterOpen));
    }

    /// MC/DC vector (obligation `select_419`): leaf A (`right.is_none()`)
    /// false -- a `JOIN` supplies a right cursor, so the gate declines
    /// and the sorter is used; flips against
    /// `mcdc__select_419__v1_single_table_bare_columns_walks_index`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__select_419__v2_join_declines_index_ordered_scan() {
        let schema = indexed_schema(&["a"], "a");
        let right = schema_named("u", &["a"]);
        let query = query("SELECT t.a FROM t JOIN u ON t.a = u.a ORDER BY t.a");
        let ops: Vec<Opcode> = compile_select_join(&schema, 0, &right, 1, &query)
            .unwrap()
            .instructions
            .iter()
            .map(|i| i.opcode)
            .collect();
        assert!(ops.contains(&Opcode::SorterOpen));
        assert!(!ops.contains(&Opcode::IdxRewind));
    }

    /// MC/DC vector (obligation `select_419`): leaf B
    /// (`!has_projected_expr`) false -- a computed SELECT-list item
    /// declines the gate on a single table; flips against
    /// `mcdc__select_419__v1_single_table_bare_columns_walks_index`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__select_419__v3_projected_expression_declines_index_ordered_scan() {
        let schema = indexed_schema(&["a"], "a");
        let ops = opcodes(&schema, &query("SELECT a + 1 FROM t ORDER BY a"));
        assert!(ops.contains(&Opcode::SorterOpen));
        assert!(!ops.contains(&Opcode::IdxRewind));
    }
}
