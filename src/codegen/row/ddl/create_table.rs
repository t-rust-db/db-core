// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `CreateTable` AST -> `Program` compilation (#215). A single
//! `Opcode::CreateTable` instruction does the entire job procedurally at
//! exec time (allocate the root page, register the row in
//! `sqlite_master`, bump the schema cookie) — there is no per-row cursor
//! work for a DDL statement, so the usual `Init -> ... -> Halt` scan
//! shape collapses to `Init -> CreateTable -> Halt`.
//!
//! `sqlite_master.sql` gets the **verbatim** source text of the
//! statement (sliced via `create.span`), not a reconstruction from the
//! parsed `ColumnDef`s — matching stock SQLite's own storage convention
//! and what `schema::read_schema` expects to be able to round-trip.

use crate::codegen::row::expr::is_aggregate_call;
use crate::codegen::row::select::CodegenError;
use crate::codegen::row::Emitter;
use crate::parser::ast::{
    ColumnConstraint, CreateTable, Expr, ExprKind, FunctionArgs, IndexedColumn, TableConstraint,
};
use crate::vm::row::{Instruction, Opcode, Program, P4};

/// Compiles `CREATE TABLE` into a single `Opcode::CreateTable` instruction
/// that allocates the root page, registers the verbatim source text as the
/// `sqlite_master` row, and bumps the schema cookie at exec time.
pub fn compile_create_table(create: &CreateTable, source: &str) -> Result<Program, CodegenError> {
    validate_create_table(create)?;
    let start = create.span.offset as usize;
    let end = start.saturating_add(create.span.len as usize);
    let sql = source
        .get(start..end)
        .ok_or_else(|| CodegenError::Unsupported {
            reason: "CREATE TABLE statement span out of bounds of the source text".to_string(),
        })?
        .to_string();

    let mut em = Emitter::new();
    let init_addr = em.emit(Instruction::new(Opcode::Init, 0, 0, 0));
    let body_start = em.new_label();
    em.place(body_start);
    em.patch_p2(init_addr, body_start);

    em.emit(Instruction::with_p4(
        Opcode::CreateTable,
        0,
        0,
        0,
        P4::CreateTable {
            name: create.name.clone(),
            sql,
            without_rowid: create.without_rowid,
            autoindexes: crate::schema::autoindexes(create)
                .into_iter()
                .filter(|index| !index.is_table)
                .map(|index| format!("sqlite_autoindex_{}_{}", create.name, index.number))
                .collect(),
        },
    ));
    em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
    Ok(em.finish())
}

/// The table definitions sqlite3 refuses when it prepares `CREATE TABLE`
/// (#562), checked in its order with its messages: duplicate columns,
/// `AUTOINCREMENT` off an `INTEGER PRIMARY KEY`, expressions or unknown
/// columns in `PRIMARY KEY`/`UNIQUE`, more than one primary key,
/// subqueries, aggregates or unknown functions in `CHECK`, then `WITHOUT
/// ROWID` without a primary key and `STRICT` columns without a valid
/// type.
fn validate_create_table(create: &CreateTable) -> Result<(), CodegenError> {
    let reject = |message: String| Err(CodegenError::Rejected(message));
    let mut primary_keys = 0_usize;
    let mut autoincrement = false;
    for (i, column) in create.columns.iter().enumerate() {
        if create
            .columns
            .iter()
            .take(i)
            .any(|c| c.name.eq_ignore_ascii_case(&column.name))
        {
            return reject(format!("duplicate column name: {}", column.name));
        }
        for constraint in &column.constraints {
            match constraint {
                ColumnConstraint::PrimaryKey {
                    autoincrement: auto,
                    ..
                } => {
                    primary_keys = primary_keys.saturating_add(1);
                    let integer = column
                        .type_name
                        .as_deref()
                        .is_some_and(|t| t.eq_ignore_ascii_case("INTEGER"));
                    if *auto && !integer {
                        return reject(
                            "AUTOINCREMENT is only allowed on an INTEGER PRIMARY KEY".into(),
                        );
                    }
                    autoincrement |= *auto;
                }
                ColumnConstraint::Check { expr, .. } => check_constraint(create, expr)?,
                _ => {}
            }
        }
        if primary_keys > 1 {
            return reject(format!(
                "table \"{}\" has more than one primary key",
                create.name
            ));
        }
    }
    for constraint in &create.constraints {
        match constraint {
            TableConstraint::PrimaryKey(terms) | TableConstraint::Unique(terms) => {
                for term in terms {
                    key_term(create, term)?;
                }
                if matches!(constraint, TableConstraint::PrimaryKey(_)) {
                    primary_keys = primary_keys.saturating_add(1);
                    if primary_keys > 1 {
                        return reject(format!(
                            "table \"{}\" has more than one primary key",
                            create.name
                        ));
                    }
                }
            }
            TableConstraint::Check { expr, .. } => check_constraint(create, expr)?,
        }
    }
    if create.without_rowid {
        if primary_keys == 0 {
            return reject(format!("PRIMARY KEY missing on table {}", create.name));
        }
        if autoincrement {
            return reject("AUTOINCREMENT not allowed on WITHOUT ROWID tables".into());
        }
        // The table is an index b-tree keyed by the PRIMARY KEY, which
        // INSERT, UPDATE and DELETE cannot write yet (#696): refuse it
        // rather than create a table no statement can modify.
        return Err(CodegenError::Unsupported {
            reason: "WITHOUT ROWID tables are not supported yet".to_string(),
        });
    }
    if create.strict {
        for column in &create.columns {
            match column.type_name.as_deref() {
                None => {
                    return reject(format!(
                        "missing datatype for {}.{}",
                        create.name, column.name
                    ))
                }
                Some(t)
                    if !["INT", "INTEGER", "REAL", "TEXT", "BLOB", "ANY"]
                        .iter()
                        .any(|ok| t.eq_ignore_ascii_case(ok)) =>
                {
                    return reject(format!(
                        "unknown datatype for {}.{}: \"{t}\"",
                        create.name, column.name
                    ))
                }
                Some(_) => {}
            }
        }
    }
    Ok(())
}

/// One `PRIMARY KEY`/`UNIQUE` term: a column of the table, possibly
/// parenthesized or `COLLATE`d, never an expression or a subquery.
fn key_term(create: &CreateTable, term: &IndexedColumn) -> Result<(), CodegenError> {
    if any_node(&term.expr, &mut is_subquery) {
        return Err(CodegenError::Rejected(
            "subqueries prohibited in index expressions".into(),
        ));
    }
    let mut expr = &term.expr;
    while let ExprKind::Paren(inner) | ExprKind::Collate { expr: inner, .. } = &expr.kind {
        expr = inner;
    }
    let ExprKind::Column { name, .. } = &expr.kind else {
        return Err(CodegenError::Rejected(
            "expressions prohibited in PRIMARY KEY and UNIQUE constraints".into(),
        ));
    };
    if !create
        .columns
        .iter()
        .any(|c| c.name.eq_ignore_ascii_case(name))
    {
        return Err(CodegenError::Rejected(format!("no such column: {name}")));
    }
    Ok(())
}

/// A `CHECK` body: no subquery, no aggregate, no unknown function, and
/// only the table's own columns.
fn check_constraint(create: &CreateTable, expr: &Expr) -> Result<(), CodegenError> {
    if any_node(expr, &mut is_subquery) {
        return Err(CodegenError::Rejected(
            "subqueries prohibited in CHECK constraints".into(),
        ));
    }
    let mut problem = None;
    any_node(expr, &mut |e| {
        if matches!(e.kind, ExprKind::Param(_)) {
            problem = Some("parameters prohibited in CHECK constraints".to_string());
            return true;
        }
        if let ExprKind::Column { name, .. } = &e.kind {
            let rowid = !create.without_rowid
                && ["rowid", "_rowid_", "oid"]
                    .iter()
                    .any(|r| name.eq_ignore_ascii_case(r));
            let known = rowid
                || create
                    .columns
                    .iter()
                    .any(|c| c.name.eq_ignore_ascii_case(name));
            problem = (!known).then(|| format!("no such column: {name}"));
            return problem.is_some();
        }
        let ExprKind::FunctionCall { name, args, .. } = &e.kind else {
            return false;
        };
        let arity = match args {
            FunctionArgs::Star => 0,
            FunctionArgs::List(list) => list.len(),
        };
        problem = if is_aggregate_call(name, args) {
            Some(format!("misuse of aggregate function {name}()"))
        } else {
            crate::functions::prepare_error(name, arity)
        };
        problem.is_some()
    });
    problem.map_or(Ok(()), |m| Err(CodegenError::Rejected(m)))
}

fn is_subquery(e: &Expr) -> bool {
    matches!(
        e.kind,
        ExprKind::Subquery(_)
            | ExprKind::Exists { .. }
            | ExprKind::InSubquery { .. }
            | ExprKind::InSubqueryMulti { .. }
    )
}

/// Whether `pred` holds for `expr` or any expression under it (not
/// descending into subqueries, which `pred` sees whole).
fn any_node(expr: &Expr, pred: &mut impl FnMut(&Expr) -> bool) -> bool {
    if pred(expr) {
        return true;
    }
    match &expr.kind {
        ExprKind::FunctionCall {
            args: FunctionArgs::List(list),
            ..
        }
        | ExprKind::In { list, .. } => {
            let head = match &expr.kind {
                ExprKind::In { expr: e, .. } => any_node(e, pred),
                _ => false,
            };
            head || list.iter().any(|a| any_node(a, pred))
        }
        ExprKind::Unary { expr: e, .. }
        | ExprKind::IsNull { expr: e, .. }
        | ExprKind::Cast { expr: e, .. }
        | ExprKind::Collate { expr: e, .. }
        | ExprKind::Paren(e)
        | ExprKind::InSubquery { expr: e, .. } => any_node(e, pred),
        ExprKind::Binary { lhs, rhs, .. } | ExprKind::Is { lhs, rhs, .. } => {
            any_node(lhs, pred) || any_node(rhs, pred)
        }
        ExprKind::Between {
            expr: e, lo, hi, ..
        } => any_node(e, pred) || any_node(lo, pred) || any_node(hi, pred),
        ExprKind::Like {
            expr: e,
            pattern,
            escape,
            ..
        } => {
            any_node(e, pred)
                || any_node(pattern, pred)
                || escape.as_deref().is_some_and(|x| any_node(x, pred))
        }
        ExprKind::Case {
            operand,
            whens,
            else_,
        } => {
            operand.as_deref().is_some_and(|x| any_node(x, pred))
                || whens
                    .iter()
                    .any(|(w, t)| any_node(w, pred) || any_node(t, pred))
                || else_.as_deref().is_some_and(|x| any_node(x, pred))
        }
        ExprKind::InSubqueryMulti { exprs, .. } => exprs.iter().any(|a| any_node(a, pred)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::row::error::ParseOutcome;
    use crate::vm::row::Opcode;

    fn rejected(sql: &str) -> Option<String> {
        let create = match crate::parser::row::error::parse_create_table(sql) {
            ParseOutcome::Accepted(c) => c,
            other => panic!("expected Accepted, got {other:?}"),
        };
        match compile_create_table(&create, sql) {
            Ok(_) => None,
            Err(CodegenError::Rejected(m)) => Some(m),
            Err(other) => panic!("{sql}: {other}"),
        }
    }

    // codegen_row_ddl_create_table_check_constraint_aa6dd797:
    // `matches!(e.kind, ExprKind::Param(_))` (#650).
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_row_ddl_create_table_check_constraint_aa6dd797__v1_a_parameter_in_a_check_is_refused(
    ) {
        for sql in [
            "CREATE TABLE x(a CHECK (a > ?))",
            "CREATE TABLE x(a, CHECK (? > 1))",
            "CREATE TABLE x(a, CHECK (a > ?2))",
            "CREATE TABLE x(a, CONSTRAINT c CHECK (a > :p))",
        ] {
            assert_eq!(
                rejected(sql).as_deref(),
                Some("parameters prohibited in CHECK constraints"),
                "{sql}"
            );
        }
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_row_ddl_create_table_check_constraint_aa6dd797__v2_a_check_without_one_is_fine(
    ) {
        assert_eq!(rejected("CREATE TABLE x(a CHECK (a > 1))"), None);
    }

    // codegen_row_ddl_create_table_validate_create_table_c4322884: `*auto && !integer`
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_row_ddl_create_table_validate_create_table_c4322884__v1_autoincrement_on_text_is_rejected(
    ) {
        let message = rejected("CREATE TABLE t (a TEXT PRIMARY KEY AUTOINCREMENT)");
        assert_eq!(
            message.as_deref(),
            Some("AUTOINCREMENT is only allowed on an INTEGER PRIMARY KEY")
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_row_ddl_create_table_validate_create_table_c4322884__v2_plain_text_primary_key_is_accepted(
    ) {
        assert_eq!(rejected("CREATE TABLE t (a TEXT PRIMARY KEY)"), None);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_row_ddl_create_table_validate_create_table_c4322884__v3_autoincrement_on_integer_is_accepted(
    ) {
        assert_eq!(
            rejected("CREATE TABLE t (a INTEGER PRIMARY KEY AUTOINCREMENT)"),
            None
        );
    }

    /// #562: a subquery anywhere in a CHECK body is found, under every
    /// kind of node, and a CHECK using all of them is otherwise fine.
    #[test]
    fn check_walks_every_expression_kind() {
        let subquery = "subqueries prohibited in CHECK constraints";
        for body in [
            "-(SELECT 1)",
            "(SELECT 1) IS NULL",
            "CAST((SELECT 1) AS INT)",
            "(SELECT 1) COLLATE nocase",
            "a BETWEEN 0 AND (SELECT 1)",
            "a IN (1, (SELECT 1))",
            "a LIKE 'x' ESCAPE (SELECT 'y')",
            "CASE a WHEN 1 THEN (SELECT 1) END",
            "CASE WHEN a THEN 1 ELSE (SELECT 1) END",
            "abs((SELECT 1))",
            "a IN (SELECT 1)",
            "(a, a) IN (SELECT 1, 2)",
            "EXISTS (SELECT 1)",
        ] {
            let sql = format!("CREATE TABLE x(a CHECK ({body}))");
            assert_eq!(rejected(&sql).as_deref(), Some(subquery), "{sql}");
        }
        assert_eq!(
            rejected(
                "CREATE TABLE x(a CHECK (-a IS NOT NULL AND CAST(a AS INT) COLLATE nocase \
                 BETWEEN 0 AND 9 AND a IN (1, 2) AND a LIKE 'x%' ESCAPE '!' AND \
                 CASE a WHEN 1 THEN 1 ELSE abs(a) END))"
            ),
            None
        );
        assert_eq!(
            rejected("CREATE TABLE x(a CHECK (CASE WHEN nosuch(a) THEN 1 END))").as_deref(),
            Some("no such function: nosuch")
        );
    }

    #[test]
    fn compiles_to_init_create_table_halt() {
        let sql = "CREATE TABLE t(a INTEGER, b TEXT)";
        let create = match crate::parser::row::error::parse_create_table(sql) {
            ParseOutcome::Accepted(c) => c,
            other => panic!("expected Accepted, got {other:?}"),
        };
        let program = compile_create_table(&create, sql).unwrap();

        let opcodes: Vec<Opcode> = program.instructions.iter().map(|i| i.opcode).collect();
        assert_eq!(
            opcodes,
            vec![Opcode::Init, Opcode::CreateTable, Opcode::Halt]
        );
        match &program.instructions[1].p4 {
            P4::CreateTable {
                name,
                sql: got_sql,
                without_rowid,
                autoindexes,
            } => {
                assert_eq!(name, "t");
                assert_eq!(got_sql, sql);
                assert!(!without_rowid);
                assert!(autoindexes.is_empty());
            }
            other => panic!("expected P4::CreateTable, got {other:?}"),
        }
    }

    fn autoindex_names(sql: &str) -> Vec<String> {
        let create = match crate::parser::row::error::parse_create_table(sql) {
            ParseOutcome::Accepted(c) => c,
            other => panic!("expected Accepted, got {other:?}"),
        };
        let program = compile_create_table(&create, sql).unwrap();
        match &program.instructions[1].p4 {
            P4::CreateTable { autoindexes, .. } => autoindexes.clone(),
            other => panic!("expected P4::CreateTable, got {other:?}"),
        }
    }

    /// The implicit indexes sqlite3 3.51 gives each of these tables,
    /// checked against its `sqlite_schema` (#694).
    #[test]
    fn implicit_indexes_follow_sqlite3_naming_and_dedupe() {
        for (sql, count) in [
            ("CREATE TABLE t1 (x TEXT PRIMARY KEY, y INT UNIQUE, z)", 2),
            (
                "CREATE TABLE t2 (x, y, UNIQUE(y), PRIMARY KEY(x, y), UNIQUE(x))",
                3,
            ),
            ("CREATE TABLE t3 (x INTEGER PRIMARY KEY, y UNIQUE)", 1),
            ("CREATE TABLE t4 (x TEXT PRIMARY KEY, UNIQUE(x))", 1),
            (
                "CREATE TABLE t6 (x, y, PRIMARY KEY(x), UNIQUE(x COLLATE NOCASE))",
                2,
            ),
            ("CREATE TABLE t8 (x INT PRIMARY KEY)", 1),
            ("CREATE TABLE t9 (x INTEGER PRIMARY KEY DESC)", 1),
            ("CREATE TABLE t10 (x, PRIMARY KEY(x DESC))", 1),
            // Table-level DESC does not stop a rowid alias; the column
            // constraint form (`t9`) does.
            ("CREATE TABLE t18 (a INTEGER, b, PRIMARY KEY(a DESC))", 0),
            (
                "CREATE TABLE t11 (a, b, UNIQUE(a,b), UNIQUE(b,a), UNIQUE(a,b))",
                2,
            ),
            (
                "CREATE TABLE t12 (a TEXT COLLATE NOCASE PRIMARY KEY, b UNIQUE)",
                2,
            ),
            ("CREATE TABLE t13 (a INTEGER, b, PRIMARY KEY(a))", 0),
            ("CREATE TABLE t14 (a INTEGER, b, PRIMARY KEY(a, b))", 1),
            ("CREATE TABLE t15 (a, UNIQUE(a), PRIMARY KEY(a))", 1),
            ("CREATE TABLE t17 (a, b)", 0),
        ] {
            let table = sql.split_whitespace().nth(2).unwrap();
            let names = autoindex_names(sql);
            assert_eq!(names.len(), count, "{sql}: {names:?}");
            assert!(
                names
                    .iter()
                    .all(|n| n.starts_with(&format!("sqlite_autoindex_{table}_"))),
                "{sql}: {names:?}"
            );
        }
        assert_eq!(
            autoindex_names("CREATE TABLE t1 (x TEXT PRIMARY KEY, y INT UNIQUE, z)"),
            vec!["sqlite_autoindex_t1_1", "sqlite_autoindex_t1_2"]
        );
    }
}
