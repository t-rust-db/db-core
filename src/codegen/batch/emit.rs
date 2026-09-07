//! `BatchExecutor` ahead-of-time Rust-source emitter (see
//! [`super`]'s module docs for where this fits against the planner) --
//! extracted from column-rs's private `src/codegen.rs` (#98/#101/#103),
//! so any `crate::vm::batch` consumer compiling queries ahead of time
//! can depend on this instead of reimplementing it.
//!
//! Renders an already-planned query to standalone Rust source text, two
//! shapes:
//!
//! - Flat/`GROUP BY`/`ORDER BY`/`LIMIT` queries compile to a `const
//!   PROGRAM: &[Opcode]` -- no runtime SQL parsing, no dynamic dispatch,
//!   the query plan is baked into the binary as data ([`render_flat`]).
//! - `JOIN` and `IN (SELECT ...)` semi-joins bypass the VM program
//!   entirely at runtime too (the generated code calls back into the
//!   caller crate's own `execute_joined`/`execute_semi_join`, which
//!   materialize whole tables and hash-join in plain Rust), so there's no
//!   single `Opcode` array to emit for them. Instead, codegen
//!   reconstructs the parsed [`Select`] (#153: `parser::ast::Select`, not
//!   a private lowered type) as a literal Rust value -- built from
//!   `String`/`Vec` constructors, not `const`, but still no SQL *text*
//!   parsed at runtime ([`render_joined`]/[`render_semi_join`]).
//! - Window functions (`SELECT`s containing `ROW_NUMBER`/`RANK`/`LAG`/
//!   etc.) bypass the VM the same way `JOIN` does: codegen reconstructs
//!   the parsed `Select` as a literal Rust value and the generated code
//!   calls the caller crate's own `execute_windowed` at runtime -- no
//!   `const PROGRAM`, since window evaluation partitions/sorts entirely
//!   outside the register-machine model ([`render_windowed`]).
//!
//! Planning -- deciding which of the above shapes a [`Select`] needs and
//! producing the flat shape's [`Program`] -- is [`crate::codegen::batch`]'s
//! job; [`generate`] calls it and then renders. The render functions only
//! turn already-planned data into text.
//!
//! **`crate_name`:** every render function takes the caller's own crate
//! name (column-rs passes `"column_rs"`) and emits `use
//! {crate_name}::...`/`{crate_name}::sql::...` etc. in the generated
//! source -- so the emitted code calls back into whichever crate actually
//! has the `ParquetFile`/`execute_joined`/`execute_windowed`/`run_program`
//! runtime glue, not a name hardcoded to column-rs specifically. Since
//! #153, the reconstructed literal is `parser::ast::Select`-shaped, so
//! the caller crate's own `sql` module is expected to mirror (or
//! re-export) `db_core::parser::ast`'s types under those names, not the
//! retired `expr::Query` module shape.

// Every `write!` here targets a `String`, which cannot fail; the discarded
// `fmt::Result` is the idiom, not a swallowed error.
#![allow(
    clippy::let_underscore_must_use,
    reason = "fmt::Write into String is infallible"
)]

use crate::codegen::batch::{compile, output_column_names};
use crate::parser::ast::{
    BinaryOp as AstBinOp, Distinctness, Expr as AstExpr, ExprKind, FromClause, FunctionArgs, Join,
    JoinConstraint, JoinOp, Limit, Literal as AstLiteral, OrderingTerm, ResultColumn, Select,
    TableRef, TableRefKind, UnaryOp, WindowDef,
};
use crate::parser::ParseError;
use crate::vm::batch::{AggFunc, AggPart, MapOp, Opcode, Program, Value};
use std::fmt::Write as _;

#[derive(Debug)]
/// Failures when emitting a batch [`Program`] from SQL text.
pub enum EmitError {
    /// The SQL text did not parse.
    Parse(ParseError),
    /// The query parsed but uses a construct this emitter does not handle yet.
    Unsupported(&'static str),
}

impl std::fmt::Display for EmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmitError::Parse(e) => write!(f, "{e}"),
            EmitError::Unsupported(what) => write!(f, "codegen does not support {what} yet"),
        }
    }
}

impl std::error::Error for EmitError {}

impl From<ParseError> for EmitError {
    fn from(e: ParseError) -> Self {
        EmitError::Parse(e)
    }
}

/// Result alias for emitter operations, with [`EmitError`] as the error type.
pub type Result<T> = std::result::Result<T, EmitError>;

/// Whether `select`'s `SELECT` list contains a window-function call
/// (`func(...) OVER (...)`).
fn has_window(select: &Select) -> bool {
    select.columns.iter().any(|c| {
        matches!(
            c,
            ResultColumn::Expr {
                expr: AstExpr {
                    kind: ExprKind::FunctionCall { over: Some(_), .. },
                    ..
                },
                ..
            }
        )
    })
}

/// `select`'s single-column `IN (SELECT ...)` `WHERE` clause, if its
/// entire `WHERE` clause is exactly that shape.
fn semi_join_subquery(select: &Select) -> Option<&Select> {
    match &select.where_clause {
        Some(AstExpr {
            kind:
                ExprKind::InSubquery {
                    expr,
                    subquery,
                    negated: false,
                },
            ..
        }) if matches!(expr.kind, ExprKind::Column { .. }) => Some(subquery),
        _ => None,
    }
}

fn from_table_name(from: &FromClause) -> &str {
    match &from.first.kind {
        TableRefKind::Name(name) => name,
        TableRefKind::Subquery(_) => from.first.alias.as_deref().unwrap_or(""),
    }
}

/// Compile `sql_text` ahead of time into a standalone `.rs` source file for
/// `crate_name`'s runtime glue (column-rs passes `"column_rs"`): plans the
/// query with [`crate::codegen::batch`], then renders the shape it needs
/// -- `const PROGRAM` for flat queries ([`render_flat`]), a reconstructed
/// `Select` literal for joins/semi-joins/windows.
pub fn generate(crate_name: &str, sql_text: &str) -> Result<String> {
    let select = crate::parser::parse(sql_text)?;
    if has_window(&select) {
        return Ok(render_windowed(crate_name, sql_text, &select));
    }

    let join_count = select
        .from
        .as_ref()
        .map(|f| f.joins.len())
        .unwrap_or_default();
    if join_count > 0 {
        if join_count > 1 {
            return Err(EmitError::Unsupported("more than one JOIN"));
        }
        return Ok(render_joined(crate_name, sql_text, &select));
    }
    if let Some(subquery) = semi_join_subquery(&select) {
        let subquery_from = subquery
            .from
            .as_ref()
            .map(from_table_name)
            .unwrap_or_default();
        return Ok(render_semi_join(
            crate_name,
            sql_text,
            &select,
            subquery_from,
        ));
    }

    let program = compile(&select);
    let columns = output_column_names(&select);
    let from = select
        .from
        .as_ref()
        .map(from_table_name)
        .unwrap_or_default();
    Ok(render_flat(crate_name, sql_text, from, &program, &columns))
}

/// A path or a simple `*`-glob (one wildcard, in the file name only --
/// e.g. `data/*.parquet`) expanded against the filesystem, sorted for
/// deterministic output. A literal path with no `*` is returned as-is
/// without touching the filesystem, so a nonexistent literal path still
/// surfaces its read error normally rather than silently expanding to
/// nothing. Embedded verbatim in every generated program's source (via
/// [`EXPAND_PATH_HELPER`]).
const EXPAND_PATH_HELPER: &str = r#"fn expand_path(pattern: &str) -> Vec<std::path::PathBuf> {
    if !pattern.contains('*') {
        return vec![std::path::PathBuf::from(pattern)];
    }
    let path = std::path::Path::new(pattern);
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| std::path::Path::new("."));
    let file_pattern = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let (prefix, suffix) = file_pattern.split_once('*').unwrap_or((file_pattern, ""));
    let mut matches: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(prefix) && n.ends_with(suffix) && n.len() >= prefix.len() + suffix.len()))
        .collect();
    matches.sort();
    matches
}
"#;

/// Render a flat/`GROUP BY`/`ORDER BY`/`LIMIT` query: a standalone `.rs`
/// source file with `const PROGRAM` (the planned VM program, including its
/// terminal `Combine`/`Sort`/`Limit` sequence (db-core#48) -- the
/// instruction stream is the whole plan, so there are no sidecar
/// `AGG_PARTS`/`ORDER_BY`/`LIMIT` consts and
/// the columns to load are derived from it at runtime), `const COLUMNS`
/// (the output column names), and a `main` that reads every Parquet file
/// path given on the command line, runs `PROGRAM` against each via the
/// caller crate's own `query::run_program`, and prints the results.
pub fn render_flat(
    crate_name: &str,
    sql_text: &str,
    table: &str,
    program: &Program,
    columns: &[String],
) -> String {
    let mut out = String::new();
    let version = crate::VERSION;
    let _ = writeln!(
        out,
        "//! Generated by db-core emit v{version} -- DO NOT EDIT"
    );
    let _ = writeln!(out, "//! Query: {}", sql_text.replace('\n', " "));
    let _ = writeln!(out, "//! Table: {table}");
    out.push_str("#![forbid(unsafe_code)]\n\n");
    out.push_str("#![allow(unused_imports)]\n");
    let _ = writeln!(out, "use {crate_name}::file::ParquetFile;");
    let _ = writeln!(out, "use {crate_name}::sql::AggFunc;");
    let _ = writeln!(
        out,
        "use {crate_name}::vm::{{AggPart, MapOp, Opcode, Value}};\n"
    );

    out.push_str("const PROGRAM: &[Opcode] = &[\n");
    for instruction in &program.instructions {
        match &instruction.comment {
            Some(comment) => {
                let _ = writeln!(
                    out,
                    "    {}, // {}",
                    render_opcode(&instruction.opcode),
                    comment.replace('\n', " ")
                );
            }
            None => {
                let _ = writeln!(out, "    {},", render_opcode(&instruction.opcode));
            }
        }
    }
    out.push_str("];\n\n");

    out.push_str("const COLUMNS: &[&str] = &[");
    for name in columns {
        let _ = write!(out, "{}, ", rust_str_literal(name));
    }
    out.push_str("];\n\n");

    out.push_str("fn main() -> Result<(), Box<dyn std::error::Error>> {\n");
    out.push_str("    let args: Vec<_> = std::env::args().skip(1).collect();\n");
    out.push_str("    if args.is_empty() {\n");
    out.push_str("        eprintln!(\"usage: {} <file.parquet>...\", std::env::args().next().unwrap_or_default());\n");
    out.push_str("        std::process::exit(1);\n");
    out.push_str("    }\n\n");
    out.push_str("    println!(\"{}\", COLUMNS.join(\"\\t\"));\n");
    out.push_str("    for pattern in &args {\n");
    out.push_str("    for path in expand_path(pattern) {\n");
    out.push_str("        let data = std::fs::read(&path)?;\n");
    out.push_str("        let file = ParquetFile::open(&data)?;\n");
    let _ = writeln!(
        out,
        "        let rows = {crate_name}::query::run_program(&file, PROGRAM)?;"
    );
    out.push_str("        for row in rows {\n");
    out.push_str(
        "            let line: Vec<String> = row.iter().map(|v| v.to_string()).collect();\n",
    );
    out.push_str("            println!(\"{}\", line.join(\"\\t\"));\n");
    out.push_str("        }\n");
    out.push_str("    }\n");
    out.push_str("    }\n");
    out.push_str("    Ok(())\n");
    out.push_str("}\n\n");
    out.push_str(EXPAND_PATH_HELPER);
    out
}

fn render_agg_part(part: &AggPart) -> String {
    match part {
        AggPart::GroupKey => "AggPart::GroupKey".to_string(),
        AggPart::Sum => "AggPart::Sum".to_string(),
        AggPart::Count => "AggPart::Count".to_string(),
        AggPart::Min => "AggPart::Min".to_string(),
        AggPart::Max => "AggPart::Max".to_string(),
        AggPart::Avg(sum, count) => format!("AggPart::Avg({sum}, {count})"),
    }
}

/// Render a `JOIN` query: a `main()` that opens the two named tables
/// (matched to `select.from`/the join's table by file stem), reconstructs
/// `select` as a literal [`Select`] value (built at runtime via ordinary
/// `Vec`/`String` constructors, not parsed from SQL text), and calls the
/// caller crate's own `execute_joined`.
pub fn render_joined(crate_name: &str, sql_text: &str, select: &Select) -> String {
    let other_table = select
        .from
        .as_ref()
        .and_then(|f| f.joins.first())
        .and_then(|j| j.table.name())
        .unwrap_or_default()
        .to_string();
    render_multi_table(crate_name, sql_text, select, &other_table, "execute_joined")
}

/// Render an `IN (SELECT ...)` semi-join query the same way as
/// [`render_joined`], but calling the caller crate's own
/// `execute_semi_join` with the subquery's table as the second table.
pub fn render_semi_join(
    crate_name: &str,
    sql_text: &str,
    select: &Select,
    subquery_from: &str,
) -> String {
    render_multi_table(
        crate_name,
        sql_text,
        select,
        subquery_from,
        "execute_semi_join",
    )
}

fn render_multi_table(
    crate_name: &str,
    sql_text: &str,
    select: &Select,
    other_table: &str,
    exec_fn: &str,
) -> String {
    let columns = output_column_names(select);
    let main_table = select
        .from
        .as_ref()
        .map(from_table_name)
        .unwrap_or_default();
    let mut out = String::new();
    let version = crate::VERSION;
    let _ = writeln!(
        out,
        "//! Generated by db-core emit v{version} -- DO NOT EDIT"
    );
    let _ = writeln!(out, "//! Query: {}", sql_text.replace('\n', " "));
    let _ = writeln!(out, "//! Tables: {main_table}, {other_table}");
    out.push_str("#![forbid(unsafe_code)]\n\n");
    out.push_str("#![allow(unused_imports)]\n");
    let _ = writeln!(out, "use {crate_name}::file::ParquetFile;");
    let _ = writeln!(
        out,
        "use {crate_name}::sql::{{BinaryOp, Distinctness, Expr, ExprKind, FromClause, FunctionArgs, Join, JoinConstraint, JoinOp, Limit, Literal, OrderingTerm, ResultColumn, Select, Span, TableRef, TableRefKind, UnaryOp}};\n"
    );

    out.push_str("const COLUMNS: &[&str] = &[");
    for name in &columns {
        let _ = write!(out, "{}, ", rust_str_literal(name));
    }
    out.push_str("];\n\n");

    let _ = writeln!(
        out,
        "const MAIN_TABLE: &str = {};",
        rust_str_literal(main_table)
    );
    let _ = writeln!(
        out,
        "const OTHER_TABLE: &str = {};\n",
        rust_str_literal(other_table)
    );

    let _ = writeln!(
        out,
        "fn build_query() -> Select {{\n    {}\n}}\n",
        render_select(select)
    );

    out.push_str("fn main() -> Result<(), Box<dyn std::error::Error>> {\n");
    out.push_str("    let args: Vec<_> = std::env::args().skip(1).collect();\n");
    out.push_str("    if args.is_empty() {\n");
    out.push_str("        eprintln!(\"usage: {} <file-or-glob>... (must cover tables '{}' and '{}')\", std::env::args().next().unwrap_or_default(), MAIN_TABLE, OTHER_TABLE);\n");
    out.push_str("        std::process::exit(1);\n");
    out.push_str("    }\n\n");
    out.push_str("    let mut tables: std::collections::HashMap<String, std::path::PathBuf> = std::collections::HashMap::new();\n");
    out.push_str("    for pattern in &args {\n");
    out.push_str("        for path in expand_path(pattern) {\n");
    out.push_str("            let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or(\"data\").to_string();\n");
    out.push_str("            tables.insert(name, path);\n");
    out.push_str("        }\n");
    out.push_str("    }\n\n");
    out.push_str("    let main_path = tables.get(MAIN_TABLE).ok_or_else(|| format!(\"no file given for table '{MAIN_TABLE}'\"))?;\n");
    out.push_str("    let other_path = tables.get(OTHER_TABLE).ok_or_else(|| format!(\"no file given for table '{OTHER_TABLE}'\"))?;\n");
    out.push_str("    let main_data = std::fs::read(main_path)?;\n");
    out.push_str("    let other_data = std::fs::read(other_path)?;\n");
    out.push_str("    let main_file = ParquetFile::open(&main_data)?;\n");
    out.push_str("    let other_file = ParquetFile::open(&other_data)?;\n");
    out.push_str("    let query = build_query();\n");
    let _ = writeln!(
        out,
        "    let rows = {crate_name}::query::{exec_fn}(&main_file, &other_file, &query)?;"
    );
    out.push_str("    println!(\"{}\", COLUMNS.join(\"\\t\"));\n");
    out.push_str("    for row in rows {\n");
    out.push_str("        let line: Vec<String> = row.iter().map(|v| v.to_string()).collect();\n");
    out.push_str("        println!(\"{}\", line.join(\"\\t\"));\n");
    out.push_str("    }\n");
    out.push_str("    Ok(())\n");
    out.push_str("}\n\n");
    out.push_str(EXPAND_PATH_HELPER);
    out
}

/// Render a window-function query: a `main()` that opens the one named
/// table, reconstructs `select` as a literal [`Select`] value (same
/// `Vec`/`String`-constructor approach as [`render_joined`]), and calls
/// the caller crate's own `execute_windowed`.
pub fn render_windowed(crate_name: &str, sql_text: &str, select: &Select) -> String {
    let columns = output_column_names(select);
    let table = select
        .from
        .as_ref()
        .map(from_table_name)
        .unwrap_or_default();
    let mut out = String::new();
    let version = crate::VERSION;
    let _ = writeln!(
        out,
        "//! Generated by db-core emit v{version} -- DO NOT EDIT"
    );
    let _ = writeln!(out, "//! Query: {}", sql_text.replace('\n', " "));
    let _ = writeln!(out, "//! Table: {table}");
    out.push_str("#![forbid(unsafe_code)]\n\n");
    out.push_str("#![allow(unused_imports)]\n");
    let _ = writeln!(out, "use {crate_name}::file::ParquetFile;");
    let _ = writeln!(
        out,
        "use {crate_name}::sql::{{BinaryOp, Distinctness, Expr, ExprKind, FromClause, FunctionArgs, Join, JoinConstraint, JoinOp, Limit, Literal, OrderingTerm, ResultColumn, Select, Span, TableRef, TableRefKind, UnaryOp, WindowDef}};\n"
    );

    out.push_str("const COLUMNS: &[&str] = &[");
    for name in &columns {
        let _ = write!(out, "{}, ", rust_str_literal(name));
    }
    out.push_str("];\n\n");

    let _ = writeln!(
        out,
        "fn build_query() -> Select {{\n    {}\n}}\n",
        render_select(select)
    );

    out.push_str("fn main() -> Result<(), Box<dyn std::error::Error>> {\n");
    out.push_str("    let args: Vec<_> = std::env::args().skip(1).collect();\n");
    out.push_str("    if args.is_empty() {\n");
    out.push_str("        eprintln!(\"usage: {} <file.parquet>...\", std::env::args().next().unwrap_or_default());\n");
    out.push_str("        std::process::exit(1);\n");
    out.push_str("    }\n\n");
    out.push_str("    println!(\"{}\", COLUMNS.join(\"\\t\"));\n");
    out.push_str("    for pattern in &args {\n");
    out.push_str("    for path in expand_path(pattern) {\n");
    out.push_str("        let data = std::fs::read(&path)?;\n");
    out.push_str("        let file = ParquetFile::open(&data)?;\n");
    out.push_str("        let query = build_query();\n");
    let _ = writeln!(
        out,
        "        let rows = {crate_name}::query::execute_windowed(&file, &query)?;"
    );
    out.push_str("        for row in rows {\n");
    out.push_str(
        "            let line: Vec<String> = row.iter().map(|v| v.to_string()).collect();\n",
    );
    out.push_str("            println!(\"{}\", line.join(\"\\t\"));\n");
    out.push_str("        }\n");
    out.push_str("    }\n");
    out.push_str("    }\n");
    out.push_str("    Ok(())\n");
    out.push_str("}\n\n");
    out.push_str(EXPAND_PATH_HELPER);
    out
}

fn render_option_str(s: Option<&str>) -> String {
    match s {
        Some(s) => format!("Some({}.to_string())", rust_str_literal(s)),
        None => "None".to_string(),
    }
}

fn render_span() -> &'static str {
    "Span::default()"
}

fn render_table_ref_kind(kind: &TableRefKind) -> String {
    match kind {
        TableRefKind::Name(name) => {
            format!("TableRefKind::Name({}.to_string())", rust_str_literal(name))
        }
        TableRefKind::Subquery(select) => {
            format!(
                "TableRefKind::Subquery(Box::new({}))",
                render_select(select)
            )
        }
    }
}

fn render_table_ref(table: &TableRef) -> String {
    format!(
        "TableRef {{ kind: {}, alias: {}, span: {} }}",
        render_table_ref_kind(&table.kind),
        render_option_str(table.alias.as_deref()),
        render_span()
    )
}

fn render_join_op(op: JoinOp) -> &'static str {
    match op {
        JoinOp::Inner => "Inner",
        JoinOp::Left => "Left",
        JoinOp::Cross => "Cross",
        JoinOp::Right => "Right",
        JoinOp::Full => "Full",
    }
}

fn render_join_constraint(constraint: &JoinConstraint) -> String {
    match constraint {
        JoinConstraint::On(expr) => format!("JoinConstraint::On({})", render_expr(expr)),
        JoinConstraint::Using(cols) => format!(
            "JoinConstraint::Using(vec![{}])",
            cols.iter()
                .map(|c| format!("{}.to_string()", rust_str_literal(c)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn render_join(join: &Join) -> String {
    format!(
        "Join {{ op: JoinOp::{}, table: {}, constraint: {}, natural: {} }}",
        render_join_op(join.op),
        render_table_ref(&join.table),
        match &join.constraint {
            Some(c) => format!("Some({})", render_join_constraint(c)),
            None => "None".to_string(),
        },
        join.natural,
    )
}

fn render_from_clause(from: &FromClause) -> String {
    let joins: Vec<String> = from.joins.iter().map(render_join).collect();
    format!(
        "FromClause {{ first: {}, joins: vec![{}] }}",
        render_table_ref(&from.first),
        joins.join(", ")
    )
}

fn render_distinctness(d: Option<Distinctness>) -> String {
    match d {
        Some(Distinctness::Distinct) => "Some(Distinctness::Distinct)".to_string(),
        Some(Distinctness::All) => "Some(Distinctness::All)".to_string(),
        None => "None".to_string(),
    }
}

fn render_select(select: &Select) -> String {
    let columns: Vec<String> = select.columns.iter().map(render_result_column).collect();
    let group_by: Vec<String> = select.group_by.iter().map(render_expr).collect();
    let order_by: Vec<String> = select.order_by.iter().map(render_ordering_term).collect();
    let from = match &select.from {
        Some(f) => format!("Some({})", render_from_clause(f)),
        None => "None".to_string(),
    };
    let where_clause = match &select.where_clause {
        Some(e) => format!("Some({})", render_expr(e)),
        None => "None".to_string(),
    };
    let limit = match &select.limit {
        Some(l) => format!("Some({})", render_limit(l)),
        None => "None".to_string(),
    };
    format!(
        "Select {{ with_clause: None, distinct: {}, columns: vec![{}], from: {}, where_clause: {}, group_by: vec![{}], having: None, compound: vec![], order_by: vec![{}], limit: {}, span: {} }}",
        render_distinctness(select.distinct),
        columns.join(", "),
        from,
        where_clause,
        group_by.join(", "),
        order_by.join(", "),
        limit,
        render_span(),
    )
}

fn render_result_column(col: &ResultColumn) -> String {
    match col {
        ResultColumn::Star => "ResultColumn::Star".to_string(),
        ResultColumn::TableStar { table } => format!(
            "ResultColumn::TableStar {{ table: {}.to_string() }}",
            rust_str_literal(table)
        ),
        ResultColumn::Expr { expr, alias } => format!(
            "ResultColumn::Expr {{ expr: {}, alias: {} }}",
            render_expr(expr),
            render_option_str(alias.as_deref())
        ),
    }
}

fn render_ordering_term(term: &OrderingTerm) -> String {
    format!(
        "OrderingTerm {{ expr: {}, desc: {}, nulls_last: {} }}",
        render_expr(&term.expr),
        render_option_bool(term.desc),
        render_option_bool(term.nulls_last)
    )
}

fn render_option_bool(v: Option<bool>) -> String {
    match v {
        Some(v) => format!("Some({v})"),
        None => "None".to_string(),
    }
}

fn render_limit(limit: &Limit) -> String {
    format!(
        "Limit {{ limit: {}, offset: {} }}",
        render_expr(&limit.limit),
        match &limit.offset {
            Some(e) => format!("Some({})", render_expr(e)),
            None => "None".to_string(),
        }
    )
}

fn render_window_def(def: &WindowDef) -> String {
    let partition_by: Vec<String> = def.partition_by.iter().map(render_expr).collect();
    let order_by: Vec<String> = def.order_by.iter().map(render_ordering_term).collect();
    format!(
        "WindowDef {{ partition_by: vec![{}], order_by: vec![{}] }}",
        partition_by.join(", "),
        order_by.join(", ")
    )
}

fn render_function_args(args: &FunctionArgs) -> String {
    match args {
        FunctionArgs::Star => "FunctionArgs::Star".to_string(),
        FunctionArgs::List(list) => format!(
            "FunctionArgs::List(vec![{}])",
            list.iter().map(render_expr).collect::<Vec<_>>().join(", ")
        ),
    }
}

fn render_expr(expr: &AstExpr) -> String {
    format!(
        "Expr {{ kind: {}, span: {} }}",
        render_expr_kind(&expr.kind),
        render_span()
    )
}

fn render_expr_kind(kind: &ExprKind) -> String {
    match kind {
        ExprKind::Literal(lit) => format!("ExprKind::Literal({})", render_literal(lit)),
        ExprKind::Column {
            table,
            catalog,
            name,
        } => format!(
            "ExprKind::Column {{ table: {}, catalog: {}, name: {}.to_string() }}",
            render_option_str(table.as_deref()),
            render_option_str(catalog.as_deref()),
            rust_str_literal(name)
        ),
        ExprKind::FunctionCall {
            name,
            distinct,
            args,
            over,
        } => format!(
            "ExprKind::FunctionCall {{ name: {}.to_string(), distinct: {distinct}, args: {}, over: {} }}",
            rust_str_literal(name),
            render_function_args(args),
            match over {
                Some(w) => format!("Some(Box::new({}))", render_window_def(w)),
                None => "None".to_string(),
            }
        ),
        ExprKind::Unary { op, expr } => {
            format!("ExprKind::Unary {{ op: UnaryOp::{}, expr: Box::new({}) }}", render_unary_op(*op), render_expr(expr))
        }
        ExprKind::Binary { op, lhs, rhs } => format!(
            "ExprKind::Binary {{ op: BinaryOp::{}, lhs: Box::new({}), rhs: Box::new({}) }}",
            render_binary_op(*op),
            render_expr(lhs),
            render_expr(rhs)
        ),
        ExprKind::Is { lhs, rhs, negated } => format!(
            "ExprKind::Is {{ lhs: Box::new({}), rhs: Box::new({}), negated: {negated} }}",
            render_expr(lhs),
            render_expr(rhs)
        ),
        ExprKind::IsNull { expr, negated } => format!(
            "ExprKind::IsNull {{ expr: Box::new({}), negated: {negated} }}",
            render_expr(expr)
        ),
        ExprKind::Paren(inner) => format!("ExprKind::Paren(Box::new({}))", render_expr(inner)),
        ExprKind::InSubquery {
            expr,
            subquery,
            negated,
        } => format!(
            "ExprKind::InSubquery {{ expr: Box::new({}), subquery: Box::new({}), negated: {negated} }}",
            render_expr(expr),
            render_select(subquery)
        ),
        ExprKind::Exists { subquery, negated } => format!(
            "ExprKind::Exists {{ subquery: Box::new({}), negated: {negated} }}",
            render_select(subquery)
        ),
        // Not part of the batch planner's validated subset -- `generate`
        // never plans a query containing these, so no renderer feeds
        // this arm today. Not a feature gap to silently paper over: once
        // a real caller needs one of these, this arm should become a
        // real render, not before.
        ExprKind::Param(_)
        | ExprKind::Between { .. }
        | ExprKind::In { .. }
        | ExprKind::Like { .. }
        | ExprKind::Case { .. }
        | ExprKind::Cast { .. }
        | ExprKind::Collate { .. }
        | ExprKind::Subquery(_)
        | ExprKind::InSubqueryMulti { .. } => {
            // Rendered as a `compile_error!` in the *generated* source, so
            // if a planner ever does feed one of these here the consumer's
            // build fails loudly with the exact shape -- rather than this
            // emitter panicking (an `unreachable!` the qualified subset
            // forbids) or silently emitting a fake rendering.
            format!(
                "compile_error!(\"db-core emit does not yet render {}\")",
                format!("{kind:?}").replace('"', "'")
            )
        }
    }
}

fn render_unary_op(op: UnaryOp) -> &'static str {
    match op {
        UnaryOp::Not => "Not",
        UnaryOp::Plus => "Plus",
        UnaryOp::Minus => "Minus",
        UnaryOp::BitNot => "BitNot",
    }
}

fn render_binary_op(op: AstBinOp) -> &'static str {
    match op {
        AstBinOp::Or => "Or",
        AstBinOp::And => "And",
        AstBinOp::Eq => "Eq",
        AstBinOp::Ne => "Ne",
        AstBinOp::Lt => "Lt",
        AstBinOp::Le => "Le",
        AstBinOp::Gt => "Gt",
        AstBinOp::Ge => "Ge",
        AstBinOp::BitAnd => "BitAnd",
        AstBinOp::BitOr => "BitOr",
        AstBinOp::Shl => "Shl",
        AstBinOp::Shr => "Shr",
        AstBinOp::Add => "Add",
        AstBinOp::Sub => "Sub",
        AstBinOp::Mul => "Mul",
        AstBinOp::Div => "Div",
        AstBinOp::Mod => "Mod",
        AstBinOp::Concat => "Concat",
    }
}

fn render_literal(lit: &AstLiteral) -> String {
    match lit {
        AstLiteral::Integer(v) => format!("Literal::Integer({v})"),
        AstLiteral::Float(v) => format!("Literal::Float({v:?})"),
        AstLiteral::Str(v) => format!("Literal::Str({}.to_string())", rust_str_literal(v)),
        AstLiteral::Blob(bytes) => format!(
            "Literal::Blob(vec![{}])",
            bytes
                .iter()
                .map(u8::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        AstLiteral::Null => "Literal::Null".to_string(),
        AstLiteral::True => "Literal::True".to_string(),
        AstLiteral::False => "Literal::False".to_string(),
    }
}

fn render_opcode(op: &Opcode) -> String {
    match op {
        Opcode::LoadColumn { reg, column } => format!(
            "Opcode::LoadColumn {{ reg: {reg}, column: std::borrow::Cow::Borrowed({}) }}",
            rust_str_literal(column)
        ),
        Opcode::LoadConst { reg, value } => format!(
            "Opcode::LoadConst {{ reg: {reg}, value: {} }}",
            render_value(value)
        ),
        Opcode::Map { dst, op, a, b } => format!(
            "Opcode::Map {{ dst: {dst}, op: MapOp::{}, a: {a}, b: {b} }}",
            render_map_op(*op)
        ),
        Opcode::Filter { predicate } => format!("Opcode::Filter {{ predicate: {predicate} }}"),
        Opcode::Reduce { func, src, dst } => format!(
            "Opcode::Reduce {{ func: AggFunc::{}, src: {}, dst: {dst} }}",
            render_agg_func(*func),
            render_option_usize(*src)
        ),
        Opcode::GroupReduce {
            group_by,
            aggs,
            agg_dst,
        } => {
            let group_by = render_usize_slice(group_by);
            let agg_dst = render_usize_slice(agg_dst);
            let aggs: Vec<String> = aggs
                .iter()
                .map(|(f, s)| {
                    format!(
                        "(AggFunc::{}, {})",
                        render_agg_func(*f),
                        render_option_usize(*s)
                    )
                })
                .collect();
            format!("Opcode::GroupReduce {{ group_by: std::borrow::Cow::Borrowed(&{group_by}), aggs: std::borrow::Cow::Borrowed(&[{}]), agg_dst: std::borrow::Cow::Borrowed(&{agg_dst}) }}", aggs.join(", "))
        }
        Opcode::Scan => "Opcode::Scan".to_string(),
        Opcode::Emit { registers } => format!(
            "Opcode::Emit {{ registers: std::borrow::Cow::Borrowed(&{}) }}",
            render_usize_slice(registers)
        ),
        Opcode::NextSegment { loop_start } => {
            format!("Opcode::NextSegment {{ loop_start: {loop_start} }}")
        }
        Opcode::Halt => "Opcode::Halt".to_string(),
        Opcode::Combine {
            agg_parts,
            num_group_keys,
            distinct,
        } => {
            let parts: Vec<String> = agg_parts.iter().map(render_agg_part).collect();
            format!(
                "Opcode::Combine {{ agg_parts: std::borrow::Cow::Borrowed(&[{}]), num_group_keys: {num_group_keys}, distinct: {distinct} }}",
                parts.join(", "),
            )
        }
        Opcode::Sort { col, descending } => {
            format!("Opcode::Sort {{ col: {col}, descending: {descending} }}")
        }
        Opcode::Limit { n } => format!("Opcode::Limit {{ n: {n} }}"),
        // No caller-side planner emits these three yet -- the join/semi-
        // join/window bypass shapes ([`render_joined`]/
        // [`render_semi_join`]/[`render_windowed`]) still reconstruct a
        // `Select` literal instead of a `const PROGRAM` for those cases
        // (see this module's top doc comment), so no flat program this
        // module renders contains them. Not a feature gap to silently
        // paper over with a fake rendering: once a real planner starts
        // emitting these, this arm should become a real render_*
        // implementation at that point, not before.
        Opcode::HashBuild { .. } | Opcode::HashProbe { .. } | Opcode::Window { .. } => {
            // Same `compile_error!`-in-generated-source strategy as
            // `render_expr_kind`'s unsupported arm: loud at the consumer's
            // build, no panic here.
            format!(
                "compile_error!(\"db-core emit does not yet render {}\")",
                format!("{op:?}").replace('"', "'")
            )
        }
    }
}

fn render_usize_slice(values: &[usize]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn render_option_usize(v: Option<usize>) -> String {
    match v {
        Some(r) => format!("Some({r})"),
        None => "None".to_string(),
    }
}

fn render_map_op(op: MapOp) -> &'static str {
    match op {
        MapOp::Add => "Add",
        MapOp::Sub => "Sub",
        MapOp::Mul => "Mul",
        MapOp::Div => "Div",
        MapOp::Eq => "Eq",
        MapOp::Ne => "Ne",
        MapOp::Lt => "Lt",
        MapOp::Le => "Le",
        MapOp::Gt => "Gt",
        MapOp::Ge => "Ge",
        MapOp::And => "And",
        MapOp::Or => "Or",
        MapOp::Not => "Not",
        MapOp::IsNull => "IsNull",
        MapOp::IsNotNull => "IsNotNull",
        MapOp::Concat => "Concat",
        MapOp::Neg => "Neg",
    }
}

fn render_agg_func(func: AggFunc) -> &'static str {
    match func {
        AggFunc::Count => "Count",
        AggFunc::Sum => "Sum",
        AggFunc::Avg => "Avg",
        AggFunc::Min => "Min",
        AggFunc::Max => "Max",
    }
}

fn render_value(value: &Value) -> String {
    match value {
        Value::Int(v) => format!("Value::Int({v})"),
        Value::Float(v) => format!("Value::Float({v:?})"),
        Value::Bool(v) => format!("Value::Bool({v})"),
        Value::Str(v) => format!(
            "Value::Str(std::borrow::Cow::Borrowed({}))",
            rust_str_literal(v)
        ),
        Value::Null => "Value::Null".to_string(),
    }
}

/// Render `s` as a Rust string literal, escaping characters that would
/// otherwise break out of it.
fn rust_str_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len().saturating_add(2));
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
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
    use crate::vm::batch::Instruction;

    #[test]
    fn render_flat_emits_const_program_and_columns() {
        let program = Program::from_opcodes([
            Opcode::LoadColumn {
                reg: 0,
                column: "id".into(),
            },
            Opcode::LoadColumn {
                reg: 1,
                column: "amount".into(),
            },
            Opcode::Emit {
                registers: vec![0, 1].into(),
            },
        ]);
        let src = render_flat(
            "column_rs",
            "SELECT id, amount FROM events",
            "events",
            &program,
            &["id".to_string(), "amount".to_string()],
        );
        assert!(src.contains("const PROGRAM: &[Opcode] = &["), "{src}");
        assert!(
            src.contains("const COLUMNS: &[&str] = &[\"id\", \"amount\", ];"),
            "{src}"
        );
        assert!(
            !src.contains("COLUMNS_TO_LOAD"),
            "input columns are derived from PROGRAM, not a sidecar const: {src}"
        );
        assert!(src.contains("#![forbid(unsafe_code)]"));
        assert!(src.contains("fn main()"));
        assert!(src.contains("use column_rs::file::ParquetFile;"));
        assert!(src.contains("column_rs::query::run_program(&file, PROGRAM)"));
    }

    #[test]
    fn render_flat_supports_glob_expansion_helper() {
        let src = render_flat(
            "column_rs",
            "SELECT id FROM t",
            "t",
            &Program::default(),
            &["id".to_string()],
        );
        assert!(src.contains("fn expand_path"), "{src}");
        assert!(src.contains("for path in expand_path(pattern)"), "{src}");
    }

    #[test]
    fn render_flat_renders_group_by_agg_parts_combine_sort_and_limit_as_separate_opcodes() {
        let program = Program::new(vec![
            Instruction::with_comment(
                Opcode::Combine {
                    agg_parts: vec![AggPart::GroupKey, AggPart::Sum].into(),
                    num_group_keys: 1,
                    distinct: false,
                },
                "merge partial aggregates",
            ),
            Instruction::with_comment(
                Opcode::Sort {
                    col: 0,
                    descending: true,
                },
                "ORDER BY region DESC",
            ),
            Instruction::with_comment(Opcode::Limit { n: 10 }, "LIMIT 10"),
        ]);
        let src = render_flat(
            "column_rs",
            "SELECT region, SUM(amount) FROM t GROUP BY region ORDER BY 1 DESC LIMIT 10",
            "t",
            &program,
            &["region".to_string(), "sum".to_string()],
        );
        assert!(
            src.contains("Opcode::Combine { agg_parts: std::borrow::Cow::Borrowed(&[AggPart::GroupKey, AggPart::Sum]), num_group_keys: 1, distinct: false }, // merge partial aggregates"),
            "{src}"
        );
        assert!(
            src.contains("Opcode::Sort { col: 0, descending: true }, // ORDER BY region DESC"),
            "{src}"
        );
        assert!(
            src.contains("Opcode::Limit { n: 10 }, // LIMIT 10"),
            "{src}"
        );
        assert!(!src.contains("const AGG_PARTS"), "{src}");
    }

    // --- `generate` (moved from column-rs's `src/codegen.rs`) ---

    #[test]
    fn generates_const_program_for_a_flat_filter_query() {
        let src = generate(
            "column_rs",
            "SELECT id, amount FROM events WHERE amount > 100",
        )
        .unwrap();
        assert!(src.contains("const PROGRAM: &[Opcode] = &["), "{src}");
        assert!(
            src.contains("\"id\"") && src.contains("\"amount\""),
            "{src}"
        );
        assert!(
            src.contains("const COLUMNS: &[&str] = &[\"id\", \"amount\", ];"),
            "{src}"
        );
        assert!(src.contains("Opcode::Filter { predicate:"), "{src}");
        assert!(src.contains("#![forbid(unsafe_code)]"));
        assert!(src.contains("fn main()"));
    }

    #[test]
    fn generates_const_program_for_a_computed_select_list_expression() {
        let src = generate("column_rs", "SELECT x * 2 + 1, a || b FROM t").unwrap();
        assert!(src.contains("const PROGRAM: &[Opcode] = &["), "{src}");
        assert!(src.contains("Opcode::Map"), "{src}");
        assert!(
            src.contains("const COLUMNS: &[&str] = &[\"x * 2 + 1\", \"a || b\", ];"),
            "{src}"
        );
    }

    #[test]
    fn generated_main_supports_glob_expansion() {
        let src = generate("column_rs", "SELECT id FROM t").unwrap();
        assert!(src.contains("fn expand_path"), "{src}");
        assert!(src.contains("for path in expand_path(pattern)"), "{src}");
    }

    #[test]
    fn generates_query_literal_for_join() {
        let src = generate(
            "column_rs",
            "SELECT a.id, b.budget FROM a JOIN b ON a.id = b.id",
        )
        .unwrap();
        assert!(src.contains("fn build_query() -> Select"), "{src}");
        assert!(
            src.contains("execute_joined(&main_file, &other_file, &query)"),
            "{src}"
        );
        assert!(src.contains("const MAIN_TABLE: &str = \"a\";"), "{src}");
        assert!(src.contains("const OTHER_TABLE: &str = \"b\";"), "{src}");
        assert!(src.contains("JoinOp::Inner"), "{src}");
    }

    #[test]
    fn generates_query_literal_for_semi_join() {
        let src = generate(
            "column_rs",
            "SELECT id FROM orders WHERE region_key IN (SELECT rkey FROM regions)",
        )
        .unwrap();
        assert!(
            src.contains("execute_semi_join(&main_file, &other_file, &query)"),
            "{src}"
        );
        assert!(
            src.contains("const OTHER_TABLE: &str = \"regions\";"),
            "{src}"
        );
        assert!(src.contains("ExprKind::InSubquery"), "{src}");
    }

    #[test]
    fn rejects_more_than_one_join() {
        let err = generate(
            "column_rs",
            "SELECT a.id FROM a JOIN b ON a.id = b.id JOIN c ON a.id = c.id",
        )
        .unwrap_err();
        assert!(matches!(err, EmitError::Unsupported("more than one JOIN")));
    }

    #[test]
    fn generates_const_program_for_group_by_aggregate_query() {
        let src = generate(
            "column_rs",
            "SELECT region, SUM(amount) FROM t GROUP BY region",
        )
        .unwrap();
        assert!(
            src.contains("Opcode::Combine { agg_parts: std::borrow::Cow::Borrowed(&[AggPart::GroupKey, AggPart::Sum]), num_group_keys: 1, distinct: false }"),
            "{src}"
        );
        assert!(src.contains("Opcode::GroupReduce {"), "{src}");
        assert!(src.contains("run_program(&file, PROGRAM)"), "{src}");
    }

    #[test]
    fn generates_const_program_for_order_by_and_limit() {
        let src = generate("column_rs", "SELECT id FROM t ORDER BY id DESC LIMIT 10").unwrap();
        assert!(
            src.contains("Opcode::Sort { col: 0, descending: true }"),
            "{src}"
        );
        assert!(src.contains("Opcode::Limit { n: 10 }"), "{src}");
    }

    #[test]
    fn render_joined_reconstructs_query_literal_and_calls_execute_joined() {
        let query = crate::parser::parse("SELECT id, budget FROM a JOIN b ON a.id = b.id").unwrap();
        let src = render_joined(
            "column_rs",
            "SELECT a.id, b.budget FROM a JOIN b ON a.id = b.id",
            &query,
        );
        assert!(src.contains("fn build_query() -> Select"), "{src}");
        assert!(
            src.contains("column_rs::query::execute_joined(&main_file, &other_file, &query)"),
            "{src}"
        );
        assert!(src.contains("const MAIN_TABLE: &str = \"a\";"), "{src}");
        assert!(src.contains("const OTHER_TABLE: &str = \"b\";"), "{src}");
        assert!(src.contains("JoinOp::Inner"), "{src}");
    }

    #[test]
    fn render_semi_join_calls_execute_semi_join() {
        let query = crate::parser::parse(
            "SELECT id FROM orders WHERE region_key IN (SELECT rkey FROM regions)",
        )
        .unwrap();
        let src = render_semi_join(
            "column_rs",
            "SELECT id FROM orders WHERE region_key IN (SELECT rkey FROM regions)",
            &query,
            "regions",
        );
        assert!(
            src.contains("column_rs::query::execute_semi_join(&main_file, &other_file, &query)"),
            "{src}"
        );
        assert!(
            src.contains("const OTHER_TABLE: &str = \"regions\";"),
            "{src}"
        );
        assert!(src.contains("ExprKind::InSubquery"), "{src}");
    }

    #[test]
    fn render_windowed_reconstructs_query_literal_and_calls_execute_windowed() {
        let query = crate::parser::parse("SELECT ROW_NUMBER() OVER (ORDER BY id) FROM t").unwrap();
        let src = render_windowed(
            "column_rs",
            "SELECT ROW_NUMBER() OVER (ORDER BY id) FROM t",
            &query,
        );
        assert!(src.contains("fn build_query() -> Select"), "{src}");
        assert!(
            src.contains("column_rs::query::execute_windowed(&file, &query)"),
            "{src}"
        );
        assert!(src.contains("\"ROW_NUMBER\""), "{src}");
    }

    #[test]
    fn render_opcode_covers_every_flat_program_variant() {
        for op in [
            Opcode::LoadColumn {
                reg: 0,
                column: "a".into(),
            },
            Opcode::LoadConst {
                reg: 1,
                value: Value::Int(1),
            },
            Opcode::Map {
                dst: 2,
                op: MapOp::Add,
                a: 0,
                b: 1,
            },
            Opcode::Filter { predicate: 2 },
            Opcode::Reduce {
                func: AggFunc::Sum,
                src: Some(0),
                dst: 1,
            },
            Opcode::GroupReduce {
                group_by: vec![0].into(),
                aggs: vec![(AggFunc::Count, None)].into(),
                agg_dst: vec![1].into(),
            },
            Opcode::Scan,
            Opcode::Emit {
                registers: vec![0].into(),
            },
            Opcode::NextSegment { loop_start: 0 },
            Opcode::Halt,
            Opcode::Combine {
                agg_parts: vec![AggPart::Avg(1, 2)].into(),
                num_group_keys: 0,
                distinct: false,
            },
            Opcode::Sort {
                col: 0,
                descending: false,
            },
            Opcode::Limit { n: 10 },
        ] {
            let rendered = render_opcode(&op);
            assert!(!rendered.is_empty());
        }
    }

    #[test]
    fn render_opcode_emits_compile_error_for_unplanned_join_opcodes() {
        // No planner feeds these to the flat renderer yet, so rather than
        // a fake rendering (or a panic in this emitter) the *generated*
        // source gets a `compile_error!` naming the opcode -- the
        // consumer's build fails loudly, at the exact shape.
        let rendered = render_opcode(&Opcode::HashBuild {
            key_cols: vec![0].into(),
            payload_cols: vec![1].into(),
            table: 0,
        });
        assert!(
            rendered.starts_with("compile_error!("),
            "expected a compile_error! rendering, got: {rendered}"
        );
        assert!(rendered.contains("HashBuild"), "got: {rendered}");
    }
}
