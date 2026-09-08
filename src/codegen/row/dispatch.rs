// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Statement dispatch: keyword-sniffs a raw SQL string to pick the right
//! parser/compiler pair for one INSERT/UPDATE/DELETE/CREATE TABLE/CREATE
//! INDEX/DROP TABLE/DROP INDEX statement (#292 — moved out of the CLI
//! binary so it's usable without depending on the binary crate, e.g. by
//! a future REPL).

use std::collections::HashMap;

use crate::codegen::row::planner::Stats;
use crate::codegen::row::{Emitter, RegAlloc, TableSchema, ViewSchema};
use crate::parser::ast::{InsertSource, Select, TableRef, TableRefKind};
use crate::parser::row::error::ParseOutcome;
use crate::parser::row::error::{
    parse_analyze, parse_begin, parse_commit, parse_create_index, parse_create_table,
    parse_create_view, parse_delete, parse_drop_index, parse_drop_table, parse_explain,
    parse_insert, parse_pragma, parse_rollback, parse_select, parse_update,
};
use crate::vm::row::{Instruction, Opcode, Program, P4};

use super::{
    compile_analyze, compile_begin, compile_commit, compile_create_index, compile_create_table,
    compile_create_view, compile_delete_with_catalog, compile_drop_index, compile_drop_table,
    compile_insert, compile_pragma, compile_rollback, compile_select_compound,
    compile_select_joined, compile_select_with_catalog, compile_select_with_catalog_and_stats,
    compile_update_with_catalog, expand_with_clause, explain_query_plan, flatten_from_subqueries,
    push_down_where_predicates, resolve_from_table_schema, resolve_views, CodegenError, EqpRow,
    ExpandViews,
};

/// Failure compiling one dispatched statement — everything
/// [`compile_statement`] can fail with, folded into one error type so
/// callers (the CLI, a future REPL) don't need to know about the
/// per-statement parser/codegen error types individually.
#[derive(Debug)]
pub enum DispatchError {
    /// The statement referenced a table not present in the schema catalog.
    NoSuchTable(String),

    /// The statement referenced an index not present in the schema catalog.
    NoSuchIndex(String),

    /// The leading keyword(s) didn't match any statement kind this
    /// dispatcher knows how to parse/compile.
    Unrecognized(String),

    /// A `SELECT` (or an embedding statement) had no `FROM` clause.
    NoFromClause,

    /// Compilation of the parsed statement failed.
    Codegen(CodegenError),

    /// Parsing the statement failed.
    ParseFailed(String),
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DispatchError::NoSuchTable(name) => write!(f, "no such table: {name}"),
            DispatchError::NoSuchIndex(name) => write!(f, "no such index: {name}"),
            DispatchError::Unrecognized(kw) => {
                write!(f, "unsupported or unrecognized statement: {kw:?} ...")
            }
            DispatchError::NoFromClause => write!(f, "SELECT has no FROM clause"),
            DispatchError::Codegen(source) => write!(f, "{source}"),
            DispatchError::ParseFailed(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for DispatchError {}

impl From<CodegenError> for DispatchError {
    fn from(source: CodegenError) -> Self {
        DispatchError::Codegen(source)
    }
}

/// The first one or two whitespace-separated words of `sql`, uppercased
/// — enough to pick which statement-specific parser to hand `sql` to
/// (`CREATE TABLE` vs `CREATE INDEX`/`CREATE UNIQUE INDEX`, `DROP TABLE`
/// vs `DROP INDEX`), without re-tokenizing the whole statement twice.
///
/// Kept as `Vec<String>` (rather than `[&str; 3]`) since callers outside
/// this module (the REPL's `leading_keywords` consumer) hold onto the
/// result past `sql`'s lifetime; the per-word allocation is unavoidable
/// there. `compile_statement` below instead does its own borrowed,
/// non-allocating scan for the hot dispatch path.
pub fn leading_keywords(sql: &str) -> Vec<String> {
    sql.split_whitespace()
        .take(3)
        .map(|w| w.to_ascii_uppercase())
        .collect()
}

/// Every leading word [`compile_statement`]'s dispatch branches on, in
/// canonical uppercase spelling — the entire vocabulary [`canonical`]
/// can return.
const DISPATCH_WORDS: &[&str] = &[
    "ANALYZE", "BEGIN", "COMMIT", "CREATE", "DELETE", "DROP", "END", "EXPLAIN", "INDEX", "INSERT",
    "PRAGMA", "ROLLBACK", "SELECT", "TABLE", "UNIQUE", "UPDATE", "VIEW", "WITH",
];

/// `word`'s canonical uppercase spelling if it's one of the statement
/// keywords dispatch branches on, else `""` — a `&'static str`, so
/// [`compile_statement`] can match on borrowed string literals without
/// allocating an uppercased copy of every leading word the way
/// [`leading_keywords`] does (#590 item 8). Any word outside this fixed
/// vocabulary maps to `""`, which matches no dispatch arm and so falls
/// through to `Unrecognized` exactly as an unknown keyword did before.
fn canonical(word: &str) -> &'static str {
    DISPATCH_WORDS
        .iter()
        .copied()
        .find(|candidate| candidate.eq_ignore_ascii_case(word))
        .unwrap_or("")
}

fn parse_error<T: std::fmt::Debug>(other: ParseOutcome<T>) -> DispatchError {
    DispatchError::ParseFailed(format!("{other:?}"))
}

/// Parses `sql`, picks the compiler for its leading keyword(s), and
/// compiles it against `schemas` — the `exec <file> "<SQL>"` CLI
/// subcommand's core (#215's write-path CLI surface), shared by any
/// future caller that needs to run a single INSERT/UPDATE/DELETE/CREATE
/// TABLE/CREATE INDEX/DROP TABLE/DROP INDEX statement against a known
/// catalog.
pub fn compile_statement(
    sql: &str,
    schemas: &[TableSchema],
    views: &[ViewSchema],
) -> Result<Program, DispatchError> {
    let find_schema = |name: &str| -> Result<&TableSchema, DispatchError> {
        schemas
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| DispatchError::NoSuchTable(name.to_string()))
    };
    let find_index_root = |name: &str| -> Result<u32, DispatchError> {
        schemas
            .iter()
            .flat_map(|s| &s.indexes)
            .find(|idx| idx.name.eq_ignore_ascii_case(name))
            .map(|idx| idx.root_page)
            .ok_or_else(|| DispatchError::NoSuchIndex(name.to_string()))
    };

    let mut words = sql.split_whitespace();
    let first_word = words.next().unwrap_or("");
    let head = canonical(first_word);
    let second = canonical(words.next().unwrap_or(""));
    // Hoisted out of the match guard so this decision and the arm's own
    // `match` don't share a line (they'd collide on one MC/DC obligation
    // id -- db-core's `unit_mcdc_discharge` uniqueness check).
    let is_create_index = second == "INDEX" || second == "UNIQUE";

    match head {
        "BEGIN" => match parse_begin(sql) {
            ParseOutcome::Accepted(begin) => Ok(compile_begin(&begin)),
            other => Err(parse_error(other)),
        },
        "COMMIT" | "END" => match parse_commit(sql) {
            ParseOutcome::Accepted(commit) => Ok(compile_commit(&commit)),
            other => Err(parse_error(other)),
        },
        "ROLLBACK" => match parse_rollback(sql) {
            ParseOutcome::Accepted(rollback) => Ok(compile_rollback(&rollback)),
            other => Err(parse_error(other)),
        },
        "PRAGMA" => match parse_pragma(sql) {
            ParseOutcome::Accepted(pragma) => Ok(compile_pragma(&pragma)),
            other => Err(parse_error(other)),
        },
        "ANALYZE" => match parse_analyze(sql) {
            ParseOutcome::Accepted(analyze) => {
                let targets: Vec<&TableSchema> = match &analyze.target {
                    None => schemas.iter().collect(),
                    Some(name) => {
                        if let Some(schema) =
                            schemas.iter().find(|s| s.name.eq_ignore_ascii_case(name))
                        {
                            vec![schema]
                        } else if schemas
                            .iter()
                            .flat_map(|s| &s.indexes)
                            .any(|idx| idx.name.eq_ignore_ascii_case(name))
                        {
                            // Real SQLite also accepts `ANALYZE index-name`
                            // (analyzing just that index's owning table) —
                            // syntactically valid, but out of this MVP's
                            // scope (spec 011/Req 1), so `Unsupported`
                            // rather than the `NoSuchTable` a genuinely
                            // unknown name gets below.
                            return Err(CodegenError::Unsupported {
                                reason: format!(
                                    "ANALYZE of a single index ({name:?}) is not yet supported"
                                ),
                            }
                            .into());
                        } else {
                            return Err(DispatchError::NoSuchTable(name.clone()));
                        }
                    }
                };
                Ok(compile_analyze(&targets)?)
            }
            other => Err(parse_error(other)),
        },
        // db-core addition (#219): sqlite-rs dispatches SELECT from its
        // CLI binary (`src/bin/sqlite-rs/query.rs::compile_select_program`),
        // which db-core has no counterpart for -- so that function's pure
        // half lives here as [`compile_select_statement`] /
        // [`explain_select_statement`], and `compile_statement` covers the
        // whole statement vocabulary as a library entry point. `WITH` is a
        // `SELECT`'s CTE prefix (only `Select` carries a `with_clause`).
        "SELECT" | "WITH" => match parse_select(sql) {
            ParseOutcome::Accepted(select) => {
                compile_select_statement(&select, schemas, views, &HashMap::new())
            }
            other => Err(parse_error(other)),
        },
        "EXPLAIN" => match parse_explain(sql) {
            ParseOutcome::Accepted(explain) => {
                if !explain.query_plan {
                    return Err(CodegenError::Unsupported {
                        reason: "bare EXPLAIN (opcode listing) is not yet supported".to_string(),
                    }
                    .into());
                }
                let rows =
                    explain_select_statement(&explain.select, schemas, views, &HashMap::new())?;
                Ok(compile_eqp_program(&rows))
            }
            other => Err(parse_error(other)),
        },
        "INSERT" => match parse_insert(sql) {
            ParseOutcome::Accepted(mut insert) => {
                let schema = find_schema(&insert.table)?;
                let select_schemas: Option<Vec<TableSchema>> = match &insert.source {
                    InsertSource::Select(select) => {
                        // Same `WITH`/view expansion `compile_select_program`
                        // runs for a plain SELECT (#375/#380), so a CTE/view
                        // name in the source at least *resolves* against the
                        // catalog instead of failing with an unexplained "no
                        // such table". The INSERT codegen path below
                        // (`compile_insert`'s single-/joined-table scan)
                        // only knows how to scan a *real* table's root page,
                        // though — it doesn't yet drive `#257`'s FROM-
                        // subquery materialization the way a plain SELECT's
                        // codegen does — so a CTE/view expanding into a
                        // `TableRefKind::Subquery` here is rejected
                        // explicitly rather than falling through to
                        // `compile_insert` and failing with a confusing
                        // "invalid root page (0)" (the subquery's synthetic,
                        // rootpage-less schema).
                        let resolved_views = resolve_views(views);
                        let cte_expanded = expand_with_clause(select);
                        let expanded = cte_expanded.expand_views(&resolved_views)?;

                        let Some(from) = &expanded.from else {
                            return Err(DispatchError::NoFromClause);
                        };
                        let is_subquery = |r: &crate::parser::ast::TableRef| {
                            matches!(r.kind, TableRefKind::Subquery(_))
                        };
                        if is_subquery(&from.first)
                            || from.joins.iter().any(|j| is_subquery(&j.table))
                        {
                            return Err(CodegenError::Unsupported {
                                reason: "INSERT ... SELECT with a CTE or view source is not yet \
                                         supported"
                                    .to_string(),
                            }
                            .into());
                        }
                        let mut joined_schemas =
                            vec![resolve_from_table_schema(&from.first, schemas)?];
                        for join in &from.joins {
                            joined_schemas.push(resolve_from_table_schema(&join.table, schemas)?);
                        }
                        insert.source = InsertSource::Select(Box::new(expanded.into_owned()));
                        Some(joined_schemas)
                    }
                    InsertSource::Values(_) | InsertSource::DefaultValues => None,
                };
                Ok(compile_insert(&insert, schema, select_schemas.as_deref())?)
            }
            other => Err(parse_error(other)),
        },
        "UPDATE" => match parse_update(sql) {
            ParseOutcome::Accepted(update) => {
                let schema = find_schema(&update.table)?;
                Ok(compile_update_with_catalog(&update, schema, schemas)?)
            }
            other => Err(parse_error(other)),
        },
        "DELETE" => match parse_delete(sql) {
            ParseOutcome::Accepted(delete) => {
                let schema = find_schema(&delete.table)?;
                Ok(compile_delete_with_catalog(&delete, schema, schemas)?)
            }
            other => Err(parse_error(other)),
        },
        "CREATE" if second == "TABLE" => match parse_create_table(sql) {
            ParseOutcome::Accepted(create) => Ok(compile_create_table(&create, sql)?),
            other => Err(parse_error(other)),
        },
        "CREATE" if second == "VIEW" => match parse_create_view(sql) {
            ParseOutcome::Accepted(create) => Ok(compile_create_view(&create, sql)?),
            other => Err(parse_error(other)),
        },
        "CREATE" if is_create_index => match parse_create_index(sql) {
            ParseOutcome::Accepted(ci) => {
                let schema = find_schema(&ci.table)?;
                Ok(compile_create_index(&ci, schema, sql)?)
            }
            other => Err(parse_error(other)),
        },
        "DROP" if second == "TABLE" => match parse_drop_table(sql) {
            ParseOutcome::Accepted(drop) => {
                let schema = find_schema(&drop.name)?;
                Ok(compile_drop_table(&drop, schema)?)
            }
            other => Err(parse_error(other)),
        },
        "DROP" if second == "INDEX" => match parse_drop_index(sql) {
            ParseOutcome::Accepted(di) => {
                let root_page = find_index_root(&di.name)?;
                Ok(compile_drop_index(&di, root_page)?)
            }
            other => Err(parse_error(other)),
        },
        // Reports the statement's actual leading word (uppercased, as
        // before), not `canonical`'s `""` sentinel — this is a cold
        // path, so the one allocation is free.
        _ => Err(DispatchError::Unrecognized(first_word.to_ascii_uppercase())),
    }
}

/// The `SELECT` rewrite pipeline shared by [`compile_select_statement`]
/// and [`explain_select_statement`] -- ported from sqlite-rs
/// `src/bin/sqlite-rs/query.rs::compile_select_program` (#219): CTEs
/// (`WITH`) and catalog views are rewritten into `FROM`-subqueries, then
/// simple ones are flattened back into the enclosing query and
/// safely-movable `WHERE` conjuncts pushed into the rest.
fn rewrite_select(select: &Select, views: &[ViewSchema]) -> Result<Select, DispatchError> {
    let cte_expanded = expand_with_clause(select);
    let resolved_views = resolve_views(views);
    let expanded = cte_expanded.expand_views(&resolved_views)?;
    let mut expanded = expanded.into_owned();
    flatten_from_subqueries(&mut expanded);
    push_down_where_predicates(&mut expanded);
    Ok(expanded)
}

/// Compiles one `SELECT` against `schemas`/`views`, choosing the
/// single-table, joined, or compound entry point exactly as sqlite-rs's
/// CLI does. `stats_by_table` carries `ANALYZE` statistics per table
/// name (empty when the caller has none -- every cost decision then
/// falls back to its no-stats default).
pub fn compile_select_statement(
    select: &Select,
    schemas: &[TableSchema],
    views: &[ViewSchema],
    stats_by_table: &HashMap<String, Stats>,
) -> Result<Program, DispatchError> {
    let select = rewrite_select(select, views)?;
    let resolve_table = |table_ref: &TableRef| resolve_from_table_schema(table_ref, schemas);

    let Some(from) = &select.from else {
        let no_table = TableSchema::default();
        return Ok(compile_select_with_catalog(&select, &no_table, &[])?);
    };
    let schema = resolve_table(&from.first)?;

    let program = if !select.compound.is_empty() {
        let mut arm_schemas = Vec::with_capacity(select.compound.len());
        for arm in &select.compound {
            let Some(arm_from) = &arm.from else {
                return Err(CodegenError::NoFromClause.into());
            };
            arm_schemas.push(resolve_table(&arm_from.first)?);
        }
        compile_select_compound(&select, &schema, &arm_schemas, schemas)?
    } else if from.joins.is_empty() {
        let stats = stats_by_table
            .get(&schema.name)
            .cloned()
            .unwrap_or_default();
        compile_select_with_catalog_and_stats(&select, &schema, schemas, &stats)?
    } else {
        let mut joined_schemas = vec![schema];
        for join in &from.joins {
            joined_schemas.push(resolve_table(&join.table)?);
        }
        compile_select_joined(&select, &joined_schemas, schemas, stats_by_table)?
    };
    Ok(program)
}

/// `EXPLAIN QUERY PLAN` for one `SELECT`: the same rewrite as
/// [`compile_select_statement`], then [`explain_query_plan`] over the
/// resolved `FROM`/`JOIN` schemas.
pub fn explain_select_statement(
    select: &Select,
    schemas: &[TableSchema],
    views: &[ViewSchema],
    stats_by_table: &HashMap<String, Stats>,
) -> Result<Vec<EqpRow>, DispatchError> {
    let select = rewrite_select(select, views)?;
    let Some(from) = &select.from else {
        return Err(DispatchError::NoFromClause);
    };
    let mut joined_schemas = vec![resolve_from_table_schema(&from.first, schemas)?];
    for join in &from.joins {
        joined_schemas.push(resolve_from_table_schema(&join.table, schemas)?);
    }
    Ok(explain_query_plan(
        &select,
        &joined_schemas,
        stats_by_table,
        schemas,
    )?)
}

/// Compiles already-computed `EXPLAIN QUERY PLAN` rows into a `Program`
/// the VM can run like any other query: every row's four columns bake as
/// constants (`explain_query_plan` computed the whole plan at codegen
/// time), `ResultRow`ed in sequence, `Halt` at the end. db-core's bridge
/// (kept from its pre-#219 `eqp.rs`) from `Vec<EqpRow>` to a dispatchable
/// `Program`; sqlite-rs prints the rows from its CLI instead.
pub fn compile_eqp_program(rows: &[EqpRow]) -> Program {
    let mut em = Emitter::new();
    let mut reg = RegAlloc::new();
    for row in rows {
        let id = reg.alloc();
        em.emit(Instruction::new(Opcode::Integer, row.id, id, 0));
        let parent = reg.alloc();
        em.emit(Instruction::new(Opcode::Integer, row.parent, parent, 0));
        let notused = reg.alloc();
        em.emit(Instruction::new(Opcode::Integer, row.notused, notused, 0));
        let detail = reg.alloc();
        em.emit(Instruction::with_p4(
            Opcode::String8,
            0,
            detail,
            0,
            P4::Str(row.detail.clone()),
        ));
        em.emit(Instruction::new(Opcode::ResultRow, id, 4, 0));
    }
    em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
    em.finish()
}

#[cfg(test)]
#[allow(non_snake_case)]
mod mcdc_vectors {
    //! Tagged MC/DC vectors for this file's multi-leaf decisions
    //! (`mcdc__<file-stem>_<line>__vN`, joined to `tests/mcdc/obligations.json`
    //! by `make test-mcdc`; db-core#219/#235).

    use crate::codegen::row::dispatch::{compile_statement, DispatchError};
    use crate::codegen::row::{CodegenError, TableSchema, ViewSchema};

    fn table(name: &str, root_page: u32, columns: &[&str]) -> TableSchema {
        TableSchema {
            name: name.to_string(),
            root_page,
            columns: columns.iter().map(|c| (*c).to_string()).collect(),
            column_types: columns.iter().map(|_| "INTEGER".to_string()).collect(),
            sql: format!("CREATE TABLE {name} ({})", columns.join(", ")),
            ..Default::default()
        }
    }

    fn catalog() -> (Vec<TableSchema>, Vec<ViewSchema>) {
        let schemas = vec![
            table("t", 2, &["a"]),
            table("u", 3, &["a"]),
            table("w", 4, &["a"]),
        ];
        let views = vec![ViewSchema {
            name: "v".to_string(),
            sql: "CREATE VIEW v AS SELECT a FROM u".to_string(),
        }];
        (schemas, views)
    }

    fn is_view_source_rejection(result: Result<crate::vm::row::Program, DispatchError>) -> bool {
        matches!(
            result,
            Err(DispatchError::Codegen(CodegenError::Unsupported { reason }))
                if reason.contains("CTE or view source")
        )
    }

    // dispatch_269: `is_subquery(&from.first) || from.joins.iter().any(|j| is_subquery(&j.table))`
    #[test]
    fn mcdc__dispatch_269__v1_view_as_first_source_is_rejected() {
        let (schemas, views) = catalog();
        assert!(is_view_source_rejection(compile_statement(
            "INSERT INTO t SELECT a FROM v",
            &schemas,
            &views
        )));
    }

    #[test]
    fn mcdc__dispatch_269__v2_view_as_joined_source_is_rejected() {
        let (schemas, views) = catalog();
        assert!(is_view_source_rejection(compile_statement(
            "INSERT INTO t SELECT u.a FROM u JOIN v ON u.a = v.a",
            &schemas,
            &views
        )));
    }

    #[test]
    fn mcdc__dispatch_269__v3_plain_table_sources_pass_the_guard() {
        let (schemas, views) = catalog();
        let result = compile_statement(
            "INSERT INTO t SELECT u.a FROM u JOIN w ON u.a = w.a",
            &schemas,
            &views,
        );
        assert!(!is_view_source_rejection(result));
    }
}
