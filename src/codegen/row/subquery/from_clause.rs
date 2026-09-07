//! `FROM`-subquery schema resolution and materialization -- see
//! `super`'s module doc.

use crate::codegen::row::cond::compile_cond;
use crate::codegen::row::value::compile_value;
use crate::codegen::row::{
    CodegenError, CondTargets, Emitter, RegAlloc, Result, Scope, TableSchema, Target,
};
use crate::parser::ast::{ExprKind, FromClause, ResultColumn, Select, TableRefKind};
use crate::vm::row::{Instruction, Opcode, P4};

/// Rejects a subquery shape this materializing pass can't compile:
/// anything that isn't a plain single-table scan with an optional
/// `WHERE`. The reference rejects the same set for a subquery-*expression*
/// (its `resolve_subquery_schema`); db-core additionally rejects the
/// post-scan clauses here, since none of them is reachable through
/// `compile_select`'s per-clause machinery from inside this inlined scan.
fn reject_unsupported_shape(subquery: &Select, what: &str) -> Result<()> {
    if !super::super::joins_of(subquery).is_empty() {
        return Err(CodegenError::Unsupported {
            reason: format!("{what} whose own FROM clause has a JOIN is not yet supported"),
        });
    }
    if super::super::is_distinct(subquery)
        || !subquery.group_by.is_empty()
        || subquery.having.is_some()
        || !subquery.order_by.is_empty()
        || subquery.limit.is_some()
        || subquery.with_clause.is_some()
        || !subquery.compound.is_empty()
    {
        return Err(CodegenError::Unsupported {
            reason: format!(
                "{what} with DISTINCT/GROUP BY/HAVING/ORDER BY/LIMIT/WITH/UNION is not yet supported"
            ),
        });
    }
    Ok(())
}

/// Resolves a subquery-expression's own single-table `FROM` against
/// `outer_scope`'s catalog.
pub(super) fn resolve_subquery_schema(
    subquery: &Select,
    outer_scope: &Scope,
) -> Result<TableSchema> {
    reject_unsupported_shape(subquery, "a subquery")?;
    let Some(name) = subquery.from.as_ref().and_then(|from| from.first.name()) else {
        return Err(CodegenError::Unsupported {
            reason: "a subquery-expression's own FROM being itself a subquery is not yet supported"
                .to_string(),
        });
    };
    outer_scope
        .catalog_table(name)
        .cloned()
        .ok_or_else(|| CodegenError::Unsupported {
            reason: format!(
                "subquery references table {name:?}, which isn't visible to this compiler's catalog"
            ),
        })
}

/// The column names a `FROM`-subquery's own `SELECT` list exposes to the
/// enclosing query, used to build the synthetic [`TableSchema`] the
/// materialized result is bound into `Scope` as. Aggregate/window items
/// are rejected rather than given the reference's positional `columnN`
/// fallback: db-core's `SelectItem` has no alias to name them by, and
/// [`materialize_from_subquery`] compiles the projection through
/// `compile_value`, which cannot evaluate an aggregate.
fn subquery_output_columns(subquery: &Select, schema: &TableSchema) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for col in &subquery.columns {
        match col {
            // An alias would be the natural output name, but the
            // synthetic schema binds by position and nothing downstream
            // reads aliases yet, so it is accepted and ignored.
            ResultColumn::Expr { expr, .. } => match &expr.kind {
                ExprKind::Column { name, .. } => out.push(name.clone()),
                _ => {
                    return Err(CodegenError::Unsupported {
                        reason: "a computed column in a FROM-subquery's SELECT list is not yet \
                                 supported"
                            .to_string(),
                    })
                }
            },
            ResultColumn::Star => out.extend(schema.columns.iter().cloned()),
            ResultColumn::TableStar { table } => {
                return Err(CodegenError::Unsupported {
                    reason: format!(
                        "`{table}.*` in a FROM-subquery's SELECT list is not yet supported"
                    ),
                })
            }
        }
    }
    Ok(out)
}

/// The [`TableSchema`] the rest of codegen should treat `from` as: a
/// real catalog lookup by name, or the synthetic schema describing a
/// `FROM`-subquery's own projected columns (its `name` is the
/// subquery's mandatory alias). Mirrors the reference's
/// `resolve_from_table_schema`.
pub fn resolve_from_table_schema(
    from: Option<&FromClause>,
    catalog: &[TableSchema],
) -> Result<TableSchema> {
    // `expr::Query.from` was always present; the AST allows a `SELECT`
    // with no `FROM` at all, which none of this codegen can scan.
    let Some(from) = from else {
        return Err(CodegenError::Unsupported {
            reason: "a SELECT with no FROM clause is not yet supported".to_string(),
        });
    };
    match &from.first.kind {
        TableRefKind::Name(name) => catalog
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(name))
            .cloned()
            .ok_or_else(|| CodegenError::Unsupported {
                reason: format!("no such table: {name}"),
            }),
        TableRefKind::Subquery(subquery) => {
            reject_unsupported_shape(subquery, "a subquery in FROM")?;
            let Some(alias) = from.first.alias.clone() else {
                return Err(CodegenError::Unsupported {
                    reason: "a FROM-subquery must have an alias".to_string(),
                });
            };
            let inner = resolve_from_table_schema(subquery.from.as_ref(), catalog)?;
            let columns = subquery_output_columns(subquery, &inner)?;
            Ok(TableSchema {
                name: alias,
                column_types: vec![String::new(); columns.len()],
                columns,
                rowid_alias: None,
                root_page: 0,
                indexes: Vec::new(),
                ..Default::default()
            })
        }
    }
}

/// Materializes a `FROM`-subquery into an in-memory ephemeral table
/// opened on `dest_cursor`, so the enclosing query can then scan it
/// exactly like a real table cursor (`Rewind`/`Next`/`Column`). Drives
/// the subquery's own single-table scan inline, substituting a row sink
/// that `MakeRecord`s each projected row and `Insert`s it with a freshly
/// `Sequence`d rowid, in place of `ResultRow` -- the same substitution
/// the reference makes.
///
/// Returns the synthetic [`TableSchema`] describing the materialized
/// table's columns. Nesting to arbitrary depth falls out of the
/// recursion, as it does in the reference.
///
/// db-core#143: if an earlier call in this same statement's compile
/// already materialized a *structurally identical* `subquery` (checked
/// via `Select`'s derived `PartialEq` -- this is how
/// [`super::cte::expand_with_clause`] rewrites a CTE referenced N times:
/// N independent [`TableRefKind::Subquery`] AST clones, one per `FROM`/
/// `JOIN` site, but every clone is byte-for-byte the same query), this
/// call reuses that materialization (`OpenDup`) instead of paying to
/// re-run and re-populate the identical query again. Safe for any
/// subquery-in-FROM, not just CTEs, given what this crate can express
/// today: no correlated variables reach this materialization path (see
/// the module doc), and no volatile/non-deterministic expression exists
/// yet either, so two textually-identical subqueries are currently
/// guaranteed to produce the same rows. **This stops being true the day
/// a volatile function is added** -- whichever ticket adds the first
/// one must revisit this cache (exclude a `Select` containing it, or
/// key off genuine CTE identity instead of raw structural equality),
/// mirroring the caveat on sqlite-rs's own `cached_cte`/`cache_cte`.
pub fn materialize_from_subquery(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    subquery: &Select,
    catalog: &[TableSchema],
    dest_cursor: i32,
) -> Result<TableSchema> {
    if let Some((source_cursor, schema)) = reg.cached_cte(subquery) {
        em.emit(Instruction::new(
            Opcode::OpenDup,
            dest_cursor,
            source_cursor,
            0,
        ));
        return Ok(schema);
    }

    reject_unsupported_shape(subquery, "a subquery in FROM")?;
    let inner_schema = resolve_from_table_schema(subquery.from.as_ref(), catalog)?;
    let columns = subquery_output_columns(subquery, &inner_schema)?;
    let synthetic = TableSchema {
        name: String::new(),
        column_types: vec![String::new(); columns.len()],
        columns: columns.clone(),
        rowid_alias: None,
        root_page: 0,
        indexes: Vec::new(),
        ..Default::default()
    };

    // p5 = 1: a rowid-keyed ephemeral *table* (scannable by
    // `Rewind`/`Next`/`Column`), not the keyed index form `IN` opens.
    em.emit(Instruction {
        opcode: Opcode::OpenEphemeral,
        p1: dest_cursor,
        p2: 0,
        p3: 0,
        p4: P4::None,
        p5: 1,
        ..Instruction::new(Opcode::OpenEphemeral, dest_cursor, 0, 0)
    });

    let src_cursor = reg.alloc_cursor();
    match subquery.from.as_ref().map(|from| &from.first.kind) {
        None | Some(TableRefKind::Name(_)) => {
            em.emit(Instruction::new(
                Opcode::OpenRead,
                src_cursor,
                i32::try_from(inner_schema.root_page).map_err(|_| CodegenError::Unsupported {
                    reason: format!(
                        "root page {} does not fit in a p2 operand",
                        inner_schema.root_page
                    ),
                })?,
                0,
            ));
        }
        Some(TableRefKind::Subquery(inner)) => {
            materialize_from_subquery(em, reg, inner, catalog, src_cursor)?;
        }
    }

    let scope = Scope::single(inner_schema, src_cursor).with_catalog(catalog.to_vec());

    let end_label = em.new_label();
    let rewind_addr = em.emit(Instruction::new(Opcode::Rewind, src_cursor, 0, 0));
    em.patch_p2(rewind_addr, end_label);
    let loop_start = em.new_label();
    em.place(loop_start);
    let skip = em.new_label();

    if let Some(where_expr) = &subquery.where_clause {
        compile_cond(
            em,
            reg,
            &scope,
            where_expr,
            CondTargets::null_is_false(Target::Fallthrough, Target::Jump(skip)),
        )?;
    }

    let mut first_reg = None;
    for (i, name) in columns.iter().enumerate() {
        let r = compile_value(em, reg, &scope, &super::super::column_expr(name.clone()))?;
        match first_reg {
            None => first_reg = Some(r),
            // `MakeRecord` reads a contiguous run, so a projection that
            // doesn't compile into one can't be recorded -- the same
            // check the reference's `compile_contiguous` makes.
            Some(first) => {
                let want = first.saturating_add(i32::try_from(i).unwrap_or(i32::MAX));
                if r != want {
                    return Err(CodegenError::Unsupported {
                        reason: "a FROM-subquery's projection must compile into contiguous \
                                 registers"
                            .to_string(),
                    });
                }
            }
        }
    }
    let Some(first) = first_reg else {
        return Err(CodegenError::Unsupported {
            reason: "a FROM-subquery must project at least one column".to_string(),
        });
    };
    let count = i32::try_from(columns.len()).map_err(|_| CodegenError::Unsupported {
        reason: format!("{} columns do not fit in a p2 operand", columns.len()),
    })?;

    let rowid_reg = reg.alloc();
    em.emit(Instruction::new(
        Opcode::Sequence,
        dest_cursor,
        rowid_reg,
        0,
    ));
    let record_reg = reg.alloc();
    em.emit(Instruction::new(
        Opcode::MakeRecord,
        first,
        count,
        record_reg,
    ));
    em.emit(Instruction::new(
        Opcode::Insert,
        dest_cursor,
        rowid_reg,
        record_reg,
    ));

    em.place(skip);
    let next_addr = em.emit(Instruction::new(Opcode::Next, src_cursor, 0, 0));
    em.patch_p2(next_addr, loop_start);
    em.place(end_label);

    reg.cache_cte(subquery.clone(), dest_cursor, synthetic.clone());
    Ok(synthetic)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::codegen::row::select::compile_select_with_catalog;
    use crate::vm::row::Program;

    fn catalog() -> Vec<TableSchema> {
        vec![TableSchema {
            name: "t".to_string(),
            columns: vec!["a".to_string(), "b".to_string()],
            column_types: vec![String::new(), String::new()],
            rowid_alias: None,
            root_page: 2,
            indexes: Vec::new(),
            ..Default::default()
        }]
    }

    fn compile(sql: &str) -> Result<Program> {
        compile_select_with_catalog(&catalog(), &crate::codegen::row::testutil::select(sql))
    }

    fn opcodes(program: &Program) -> Vec<Opcode> {
        program.instructions.iter().map(|i| i.opcode).collect()
    }

    #[test]
    fn resolves_a_subquerys_projected_columns_as_a_synthetic_schema() {
        let query = crate::codegen::row::testutil::select("SELECT b FROM (SELECT b FROM t) x");
        let schema = resolve_from_table_schema(query.from.as_ref(), &catalog()).unwrap();
        assert_eq!(schema.name, "x");
        assert_eq!(schema.columns, vec!["b".to_string()]);
        assert_eq!(schema.root_page, 0);
    }

    #[test]
    fn star_projection_exposes_every_underlying_column() {
        let query = crate::codegen::row::testutil::select("SELECT a FROM (SELECT * FROM t) x");
        let schema = resolve_from_table_schema(query.from.as_ref(), &catalog()).unwrap();
        assert_eq!(schema.columns, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn materializes_into_an_ephemeral_table_when_flattening_cannot_apply() {
        // `DISTINCT` on the *outer* query is rejected by `compile_select`,
        // so an unflattenable inner `LIMIT` is what forces materialization.
        let program = compile("SELECT b FROM (SELECT b FROM t LIMIT 1) x").unwrap_err();
        assert!(matches!(program, CodegenError::Unsupported { .. }));

        let mut em = Emitter::new();
        let mut reg = RegAlloc::new();
        let inner = crate::codegen::row::testutil::select("SELECT b FROM t WHERE a = 1");
        let schema = materialize_from_subquery(&mut em, &mut reg, &inner, &catalog(), 0).unwrap();
        assert_eq!(schema.columns, vec!["b".to_string()]);
        let ops: Vec<Opcode> = em.finish().instructions.iter().map(|i| i.opcode).collect();
        assert!(ops.contains(&Opcode::OpenEphemeral), "{ops:?}");
        assert!(ops.contains(&Opcode::Sequence), "{ops:?}");
        assert!(ops.contains(&Opcode::MakeRecord), "{ops:?}");
        assert!(ops.contains(&Opcode::Insert), "{ops:?}");
    }

    #[test]
    fn a_structurally_identical_subquery_reuses_the_first_materialization() {
        let mut em = Emitter::new();
        let mut reg = RegAlloc::new();
        let inner = crate::codegen::row::testutil::select("SELECT b FROM t WHERE a = 1");
        let first = materialize_from_subquery(&mut em, &mut reg, &inner, &catalog(), 0).unwrap();
        let second = materialize_from_subquery(&mut em, &mut reg, &inner, &catalog(), 1).unwrap();
        assert_eq!(first.columns, second.columns);

        let program = em.finish();
        let open_ephemeral_count = program
            .instructions
            .iter()
            .filter(|i| i.opcode == Opcode::OpenEphemeral)
            .count();
        assert_eq!(open_ephemeral_count, 1, "{:?}", program.instructions);
        let open_dup = program
            .instructions
            .iter()
            .find(|i| i.opcode == Opcode::OpenDup)
            .unwrap_or_else(|| panic!("expected an OpenDup, got {:?}", program.instructions));
        assert_eq!(open_dup.p1, 1);
        assert_eq!(open_dup.p2, 0);
    }

    #[test]
    fn a_flattenable_from_subquery_never_opens_an_ephemeral_table() {
        let program = compile("SELECT b FROM (SELECT a, b FROM t WHERE a = 1) x").unwrap();
        let ops = opcodes(&program);
        assert!(!ops.contains(&Opcode::OpenEphemeral), "{ops:?}");
        assert!(ops.contains(&Opcode::Rewind), "{ops:?}");
    }

    #[test]
    fn star_over_a_narrowing_from_subquery_materializes() {
        // Flattening would widen `*` from the subquery's one column to
        // the whole underlying table, so it declines and the ephemeral
        // table is what carries the projection.
        let program = compile("SELECT * FROM (SELECT b FROM t) x").unwrap();
        let ops = opcodes(&program);
        assert!(ops.contains(&Opcode::OpenEphemeral), "{ops:?}");
        assert!(ops.contains(&Opcode::Insert), "{ops:?}");
        assert!(ops.contains(&Opcode::ResultRow), "{ops:?}");
    }

    #[test]
    fn an_aggregate_in_a_from_subquery_is_unsupported() {
        let q = crate::codegen::row::testutil::select("SELECT c FROM (SELECT COUNT(*) FROM t) x");
        assert!(resolve_from_table_schema(q.from.as_ref(), &catalog()).is_err());
    }

    /// The all-leaves-false baseline for `reject_unsupported_shape`'s
    /// seven-way post-scan-clause disjunction: a plain single-table
    /// scan. Each `mcdc__from_clause_24__*` vector flips exactly one
    /// field on this shape, so the leaf under test is the only one that
    /// differs from the baseline.
    fn plain_scan() -> Select {
        crate::codegen::row::testutil::select("SELECT b FROM t")
    }

    fn is_unsupported(result: Result<()>) -> bool {
        matches!(result, Err(CodegenError::Unsupported { .. }))
    }

    /// MC/DC vector (obligation `from_clause_24`, the seven-leaf
    /// DISTINCT/GROUP BY/HAVING/ORDER BY/LIMIT/WITH/compound disjunction):
    /// every leaf false -- a plain scan passes.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__from_clause_24__v1_plain_scan_passes() {
        assert!(reject_unsupported_shape(&plain_scan(), "a subquery").is_ok());
    }

    /// MC/DC vector (obligation `from_clause_24`): leaf 1 (`DISTINCT`)
    /// true alone.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__from_clause_24__v2_distinct_alone_rejects() {
        let mut q = plain_scan();
        q.distinct = Some(crate::parser::ast::Distinctness::Distinct);
        assert!(is_unsupported(reject_unsupported_shape(&q, "a subquery")));
    }

    /// MC/DC vector (obligation `from_clause_24`): leaf 2 (`GROUP BY`)
    /// true alone.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__from_clause_24__v3_group_by_alone_rejects() {
        let mut q = plain_scan();
        q.group_by = crate::codegen::row::testutil::select("SELECT b FROM t GROUP BY b").group_by;
        assert!(!q.group_by.is_empty());
        assert!(is_unsupported(reject_unsupported_shape(&q, "a subquery")));
    }

    /// MC/DC vector (obligation `from_clause_24`): leaf 3 (`HAVING`) true
    /// alone. The grammar only accepts `HAVING` alongside `GROUP BY`, so
    /// the predicate is lifted from a `WHERE` onto the baseline's
    /// `having` slot to keep `group_by` empty.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__from_clause_24__v4_having_alone_rejects() {
        let mut q = plain_scan();
        q.having =
            crate::codegen::row::testutil::select("SELECT b FROM t WHERE b > 1").where_clause;
        assert!(q.having.is_some());
        assert!(is_unsupported(reject_unsupported_shape(&q, "a subquery")));
    }

    /// MC/DC vector (obligation `from_clause_24`): leaf 4 (`ORDER BY`)
    /// true alone.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__from_clause_24__v5_order_by_alone_rejects() {
        let mut q = plain_scan();
        q.order_by = crate::codegen::row::testutil::select("SELECT b FROM t ORDER BY b").order_by;
        assert!(!q.order_by.is_empty());
        assert!(is_unsupported(reject_unsupported_shape(&q, "a subquery")));
    }

    /// MC/DC vector (obligation `from_clause_24`): leaf 5 (`LIMIT`) true
    /// alone.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__from_clause_24__v6_limit_alone_rejects() {
        let mut q = plain_scan();
        q.limit = crate::codegen::row::testutil::select("SELECT b FROM t LIMIT 1").limit;
        assert!(q.limit.is_some());
        assert!(is_unsupported(reject_unsupported_shape(&q, "a subquery")));
    }

    /// MC/DC vector (obligation `from_clause_24`): leaf 6 (`WITH`) true
    /// alone.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__from_clause_24__v7_with_clause_alone_rejects() {
        let mut q = plain_scan();
        q.with_clause =
            crate::codegen::row::testutil::select("WITH c AS (SELECT b FROM t) SELECT b FROM t")
                .with_clause;
        assert!(q.with_clause.is_some());
        assert!(is_unsupported(reject_unsupported_shape(&q, "a subquery")));
    }

    /// MC/DC vector (obligation `from_clause_24`): leaf 7 (compound
    /// `UNION ALL` arms) true alone.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__from_clause_24__v8_compound_alone_rejects() {
        let mut q = plain_scan();
        q.compound =
            crate::codegen::row::testutil::select("SELECT b FROM t UNION ALL SELECT b FROM t")
                .compound;
        assert!(!q.compound.is_empty());
        assert!(is_unsupported(reject_unsupported_shape(&q, "a subquery")));
    }
}
