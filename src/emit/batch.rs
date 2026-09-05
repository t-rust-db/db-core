//! `BatchExecutor` ahead-of-time Rust-source emitter -- one of `emit`'s
//! three emitters (see module docs) -- extracted from column-rs's private
//! `src/codegen.rs` (#98/#101/#103), so any `crate::vm::batch` consumer
//! compiling queries ahead of time can depend on this instead of
//! reimplementing it.
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
//!   reconstructs the parsed [`Query`] as a literal Rust value -- built
//!   from `String`/`Vec` constructors, not `const`, but still no SQL
//!   *text* parsed at runtime ([`render_joined`]/[`render_semi_join`]).
//! - Window functions (`SELECT`s containing `ROW_NUMBER`/`RANK`/`LAG`/
//!   etc.) bypass the VM the same way `JOIN` does: codegen reconstructs
//!   the parsed `Query` as a literal Rust value and the generated code
//!   calls the caller crate's own `execute_windowed` at runtime -- no
//!   `const PROGRAM`, since window evaluation partitions/sorts entirely
//!   outside the register-machine model ([`render_windowed`]).
//!
//! Planning -- deciding which of the above shapes a [`Query`] needs and
//! producing the flat shape's [`Program`] -- is [`crate::codegen::batch`]'s
//! job; [`generate`] calls it and then renders. The render functions only
//! turn already-planned data into text.
//!
//! **`crate_name`:** every render function takes the caller's own crate
//! name (column-rs passes `"column_rs"`) and emits `use
//! {crate_name}::...`/`{crate_name}::query::...` etc. in the generated
//! source -- so the emitted code calls back into whichever crate actually
//! has the `ParquetFile`/`execute_joined`/`execute_windowed`/`run_program`
//! runtime glue, not a name hardcoded to column-rs specifically.

// Every `write!` here targets a `String`, which cannot fail; the discarded
// `fmt::Result` is the idiom, not a swallowed error.
#![allow(
    clippy::let_underscore_must_use,
    reason = "fmt::Write into String is infallible"
)]

use crate::codegen::batch::{compile, output_column_names};
use crate::expr::{
    AggFunc, BinOp, Expr, FromClause, Join, JoinKind, OrderBy, Query, SelectItem, WindowFunc,
    WindowSpec,
};
use crate::parser::ParseError;
use crate::types::Literal;
use crate::vm::batch::{AggPart, MapOp, Opcode, Program, Value};
use std::fmt::Write as _;

#[derive(Debug)]
pub enum EmitError {
    Parse(ParseError),
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

pub type Result<T> = std::result::Result<T, EmitError>;

/// Compile `sql_text` ahead of time into a standalone `.rs` source file for
/// `crate_name`'s runtime glue (column-rs passes `"column_rs"`): plans the
/// query with [`crate::codegen::batch`], then renders the shape it needs
/// -- `const PROGRAM` for flat queries ([`render_flat`]), a reconstructed
/// `Query` literal for joins/semi-joins/windows.
pub fn generate(crate_name: &str, sql_text: &str) -> Result<String> {
    let query = crate::parser::parse(sql_text)?;
    if query
        .columns
        .iter()
        .any(|c| matches!(c, SelectItem::Window(_)))
    {
        return Ok(render_windowed(crate_name, sql_text, &query));
    }

    if !query.joins.is_empty() {
        if query.joins.len() > 1 {
            return Err(EmitError::Unsupported("more than one JOIN"));
        }
        return Ok(render_joined(crate_name, sql_text, &query));
    }
    if let Some(Expr::InSubquery { expr, subquery }) = &query.where_clause {
        let Expr::Column(_) = expr.as_ref() else {
            return Err(EmitError::Unsupported(
                "IN (SELECT ...) with a non-column left-hand side",
            ));
        };
        return Ok(render_semi_join(
            crate_name,
            sql_text,
            &query,
            subquery.from.name(),
        ));
    }

    let program = compile(&query);
    let columns = output_column_names(&query);
    Ok(render_flat(
        crate_name,
        sql_text,
        query.from.name(),
        &program,
        &columns,
    ))
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
/// terminal `Opcode::Finalize` -- the instruction stream is the whole
/// plan, so there are no sidecar `AGG_PARTS`/`ORDER_BY`/`LIMIT` consts and
/// the columns to load are derived from it at runtime), `const COLUMNS`
/// (the output column names), and a `main` that reads every Parquet file
/// path given on the command line, runs `PROGRAM` against each via the
/// caller crate's `query::run_program`, and prints the results.
pub fn render_flat(
    crate_name: &str,
    sql_text: &str,
    table: &str,
    program: &Program,
    columns: &[String],
) -> String {
    let mut out = String::new();
    let version = env!("CARGO_PKG_VERSION");
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

fn render_order_by(order_by: Option<(usize, bool)>) -> String {
    match order_by {
        Some((pos, desc)) => format!("Some(({pos}, {desc}))"),
        None => "None".to_string(),
    }
}

/// Render a `JOIN` query: a `main()` that opens the two named tables
/// (matched to `query.from`/`join.table` by file stem), reconstructs
/// `query` as a literal [`Query`] value (built at runtime via ordinary
/// `Vec`/`String` constructors, not parsed from SQL text), and calls the
/// caller crate's own `execute_joined`.
pub fn render_joined(crate_name: &str, sql_text: &str, query: &Query) -> String {
    let other_table = query.joins[0].table.clone();
    render_multi_table(crate_name, sql_text, query, &other_table, "execute_joined")
}

/// Render an `IN (SELECT ...)` semi-join query the same way as
/// [`render_joined`], but calling the caller crate's own
/// `execute_semi_join` with the subquery's table as the second table.
pub fn render_semi_join(
    crate_name: &str,
    sql_text: &str,
    query: &Query,
    subquery_from: &str,
) -> String {
    render_multi_table(
        crate_name,
        sql_text,
        query,
        subquery_from,
        "execute_semi_join",
    )
}

fn render_multi_table(
    crate_name: &str,
    sql_text: &str,
    query: &Query,
    other_table: &str,
    exec_fn: &str,
) -> String {
    let columns = output_column_names(query);
    let mut out = String::new();
    let version = env!("CARGO_PKG_VERSION");
    let _ = writeln!(
        out,
        "//! Generated by db-core emit v{version} -- DO NOT EDIT"
    );
    let _ = writeln!(out, "//! Query: {}", sql_text.replace('\n', " "));
    let _ = writeln!(out, "//! Tables: {}, {other_table}", query.from.name());
    out.push_str("#![forbid(unsafe_code)]\n\n");
    out.push_str("#![allow(unused_imports)]\n");
    let _ = writeln!(out, "use {crate_name}::file::ParquetFile;");
    let _ = writeln!(
        out,
        "use {crate_name}::sql::{{AggFunc, BinOp, Expr, FromClause, Join, JoinKind, Literal, OrderBy, Query, SelectItem}};\n"
    );

    out.push_str("const COLUMNS: &[&str] = &[");
    for name in &columns {
        let _ = write!(out, "{}, ", rust_str_literal(name));
    }
    out.push_str("];\n\n");

    let _ = writeln!(
        out,
        "const MAIN_TABLE: &str = {};",
        rust_str_literal(query.from.name())
    );
    let _ = writeln!(
        out,
        "const OTHER_TABLE: &str = {};\n",
        rust_str_literal(other_table)
    );

    let _ = writeln!(
        out,
        "fn build_query() -> Query {{\n    {}\n}}\n",
        render_query(query)
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
/// table, reconstructs `query` as a literal [`Query`] value (same
/// `Vec`/`String`-constructor approach as [`render_joined`]), and calls
/// the caller crate's own `execute_windowed`.
pub fn render_windowed(crate_name: &str, sql_text: &str, query: &Query) -> String {
    let columns = output_column_names(query);
    let mut out = String::new();
    let version = env!("CARGO_PKG_VERSION");
    let _ = writeln!(
        out,
        "//! Generated by db-core emit v{version} -- DO NOT EDIT"
    );
    let _ = writeln!(out, "//! Query: {}", sql_text.replace('\n', " "));
    let _ = writeln!(out, "//! Table: {}", query.from.name());
    out.push_str("#![forbid(unsafe_code)]\n\n");
    out.push_str("#![allow(unused_imports)]\n");
    let _ = writeln!(out, "use {crate_name}::file::ParquetFile;");
    let _ = writeln!(
        out,
        "use {crate_name}::sql::{{AggFunc, BinOp, Expr, FromClause, OrderBy, Query, SelectItem, WindowFunc, WindowSpec}};\n"
    );

    out.push_str("const COLUMNS: &[&str] = &[");
    for name in &columns {
        let _ = write!(out, "{}, ", rust_str_literal(name));
    }
    out.push_str("];\n\n");

    let _ = writeln!(
        out,
        "fn build_query() -> Query {{\n    {}\n}}\n",
        render_query(query)
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

fn render_query(query: &Query) -> String {
    let columns: Vec<String> = query.columns.iter().map(render_select_item).collect();
    let joins: Vec<String> = query.joins.iter().map(render_join).collect();
    let group_by: Vec<String> = query
        .group_by
        .iter()
        .map(|c| format!("{}.to_string()", rust_str_literal(c)))
        .collect();
    format!(
        "Query {{ columns: vec![{}], from: {}, joins: vec![{}], where_clause: {}, distinct: {}, group_by: vec![{}], having: {}, order_by: {}, limit: {}, offset: {} }}",
        columns.join(", "),
        render_from_clause(&query.from),
        joins.join(", "),
        render_option_expr(query.where_clause.as_ref()),
        query.distinct,
        group_by.join(", "),
        render_option_expr(query.having.as_ref()),
        render_option_order_by(query.order_by.as_ref()),
        render_option_usize(query.limit),
        render_option_usize(query.offset),
    )
}

fn render_select_item(item: &SelectItem) -> String {
    match item {
        SelectItem::Column(name) => {
            format!("SelectItem::Column({}.to_string())", rust_str_literal(name))
        }
        SelectItem::Star => "SelectItem::Star".to_string(),
        SelectItem::Agg(func, arg) => format!(
            "SelectItem::Agg(AggFunc::{}, {})",
            render_agg_func(*func),
            render_option_string(arg.as_deref())
        ),
        SelectItem::Window(spec) => {
            format!("SelectItem::Window({})", render_window_spec(spec))
        }
    }
}

fn render_window_spec(spec: &WindowSpec) -> String {
    let partition_by: Vec<String> = spec
        .partition_by
        .iter()
        .map(|c| format!("{}.to_string()", rust_str_literal(c)))
        .collect();
    let order_by: Vec<String> = spec
        .order_by
        .iter()
        .map(|(c, desc)| format!("({}.to_string(), {desc})", rust_str_literal(c)))
        .collect();
    format!(
        "WindowSpec {{ func: WindowFunc::{}, arg: {}, offset: {}, partition_by: vec![{}], order_by: vec![{}] }}",
        render_window_func(spec.func),
        render_option_string(spec.arg.as_deref()),
        render_option_i64(spec.offset),
        partition_by.join(", "),
        order_by.join(", "),
    )
}

fn render_window_func(func: WindowFunc) -> &'static str {
    match func {
        WindowFunc::RowNumber => "RowNumber",
        WindowFunc::Rank => "Rank",
        WindowFunc::DenseRank => "DenseRank",
        WindowFunc::Lag => "Lag",
        WindowFunc::Lead => "Lead",
        WindowFunc::FirstValue => "FirstValue",
        WindowFunc::LastValue => "LastValue",
        WindowFunc::Sum => "Sum",
        WindowFunc::Avg => "Avg",
        WindowFunc::Count => "Count",
    }
}

fn render_option_i64(v: Option<i64>) -> String {
    match v {
        Some(v) => format!("Some({v})"),
        None => "None".to_string(),
    }
}

fn render_option_string(s: Option<&str>) -> String {
    match s {
        Some(s) => format!("Some({}.to_string())", rust_str_literal(s)),
        None => "None".to_string(),
    }
}

fn render_join(join: &Join) -> String {
    let kind = match join.kind {
        JoinKind::Inner => "Inner",
        JoinKind::Left => "Left",
        JoinKind::Right => "Right",
        JoinKind::Full => "Full",
        JoinKind::Cross => "Cross",
    };
    format!(
        "Join {{ kind: JoinKind::{kind}, table: {}.to_string(), left_col: {}.to_string(), right_col: {}.to_string() }}",
        rust_str_literal(&join.table),
        rust_str_literal(&join.left_col),
        rust_str_literal(&join.right_col),
    )
}

fn render_from_clause(from: &FromClause) -> String {
    match from {
        FromClause::Table(name) => {
            format!("FromClause::Table({}.to_string())", rust_str_literal(name))
        }
        FromClause::Subquery(query, alias) => format!(
            "FromClause::Subquery(Box::new({}), {}.to_string())",
            render_query(query),
            rust_str_literal(alias)
        ),
    }
}

fn render_expr(expr: &Expr) -> String {
    match expr {
        Expr::Column(name) => format!("Expr::Column({}.to_string())", rust_str_literal(name)),
        Expr::Literal(lit) => format!("Expr::Literal({})", render_literal(lit)),
        Expr::BinaryOp(l, op, r) => format!(
            "Expr::BinaryOp(Box::new({}), BinOp::{}, Box::new({}))",
            render_expr(l),
            render_bin_op(*op),
            render_expr(r)
        ),
        Expr::InSubquery { expr, subquery } => format!(
            "Expr::InSubquery {{ expr: Box::new({}), subquery: Box::new({}) }}",
            render_expr(expr),
            render_query(subquery)
        ),
        Expr::Exists { subquery, negated } => format!(
            "Expr::Exists {{ subquery: Box::new({}), negated: {negated} }}",
            render_query(subquery)
        ),
        Expr::Not(inner) => format!("Expr::Not(Box::new({}))", render_expr(inner)),
        Expr::Neg(inner) => format!("Expr::Neg(Box::new({}))", render_expr(inner)),
        Expr::IsNull { expr, negated } => format!(
            "Expr::IsNull {{ expr: Box::new({}), negated: {negated} }}",
            render_expr(expr)
        ),
    }
}

fn render_literal(lit: &Literal) -> String {
    match lit {
        Literal::Int(v) => format!("Literal::Int({v})"),
        Literal::Float(v) => format!("Literal::Float({v:?})"),
        Literal::Str(v) => format!("Literal::Str({}.to_string())", rust_str_literal(v)),
    }
}

fn render_bin_op(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "Add",
        BinOp::Sub => "Sub",
        BinOp::Mul => "Mul",
        BinOp::Div => "Div",
        BinOp::Eq => "Eq",
        BinOp::Ne => "Ne",
        BinOp::Lt => "Lt",
        BinOp::Le => "Le",
        BinOp::Gt => "Gt",
        BinOp::Ge => "Ge",
        BinOp::And => "And",
        BinOp::Or => "Or",
        BinOp::Concat => "Concat",
    }
}

fn render_option_expr(expr: Option<&Expr>) -> String {
    match expr {
        Some(e) => format!("Some({})", render_expr(e)),
        None => "None".to_string(),
    }
}

fn render_option_order_by(ob: Option<&OrderBy>) -> String {
    match ob {
        Some(OrderBy { column, descending }) => format!(
            "Some(OrderBy {{ column: {}.to_string(), descending: {descending} }})",
            rust_str_literal(column)
        ),
        None => "None".to_string(),
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
        Opcode::Finalize {
            agg_parts,
            num_group_keys,
            distinct,
            order_by,
            limit,
        } => {
            let parts: Vec<String> = agg_parts.iter().map(render_agg_part).collect();
            format!(
                "Opcode::Finalize {{ agg_parts: std::borrow::Cow::Borrowed(&[{}]), num_group_keys: {num_group_keys}, distinct: {distinct}, order_by: {}, limit: {} }}",
                parts.join(", "),
                render_order_by(*order_by),
                render_option_usize(*limit)
            )
        }
        // No caller-side planner emits these three yet -- the join/semi-
        // join/window bypass shapes ([`render_joined`]/
        // [`render_semi_join`]/[`render_windowed`]) still reconstruct a
        // `Query` literal instead of a `const PROGRAM` for those cases
        // (see this module's top doc comment), so no flat program this
        // module renders contains them. Not a feature gap to silently
        // paper over with a fake rendering: once a real planner starts
        // emitting these, this arm should become a real render_*
        // implementation at that point, not before.
        Opcode::HashBuild { .. } | Opcode::HashProbe { .. } | Opcode::Window { .. } => {
            unreachable!("no planner feeding emit::batch emits {op:?} yet")
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
    let mut out = String::with_capacity(s.len() + 2);
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
    fn render_flat_renders_group_by_agg_parts_and_order_limit_inside_finalize() {
        let program = Program::new(vec![Instruction::with_comment(
            Opcode::Finalize {
                agg_parts: vec![AggPart::GroupKey, AggPart::Sum].into(),
                num_group_keys: 1,
                distinct: false,
                order_by: Some((0, true)),
                limit: Some(10),
            },
            "merge; ORDER BY region DESC; LIMIT 10",
        )]);
        let src = render_flat(
            "column_rs",
            "SELECT region, SUM(amount) FROM t GROUP BY region ORDER BY 1 DESC LIMIT 10",
            "t",
            &program,
            &["region".to_string(), "sum".to_string()],
        );
        assert!(
            src.contains("Opcode::Finalize { agg_parts: std::borrow::Cow::Borrowed(&[AggPart::GroupKey, AggPart::Sum]), num_group_keys: 1, distinct: false, order_by: Some((0, true)), limit: Some(10) }, // merge; ORDER BY region DESC; LIMIT 10"),
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
        assert!(src.contains("fn build_query() -> Query"), "{src}");
        assert!(
            src.contains("execute_joined(&main_file, &other_file, &query)"),
            "{src}"
        );
        assert!(src.contains("const MAIN_TABLE: &str = \"a\";"), "{src}");
        assert!(src.contains("const OTHER_TABLE: &str = \"b\";"), "{src}");
        assert!(src.contains("JoinKind::Inner"), "{src}");
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
        assert!(src.contains("Expr::InSubquery"), "{src}");
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
            src.contains("Opcode::Finalize { agg_parts: std::borrow::Cow::Borrowed(&[AggPart::GroupKey, AggPart::Sum]), num_group_keys: 1, distinct: false, order_by: None, limit: None }"),
            "{src}"
        );
        assert!(src.contains("Opcode::GroupReduce {"), "{src}");
        assert!(src.contains("run_program(&file, PROGRAM)"), "{src}");
    }

    #[test]
    fn generates_const_program_for_order_by_and_limit() {
        let src = generate("column_rs", "SELECT id FROM t ORDER BY id DESC LIMIT 10").unwrap();
        assert!(
            src.contains("order_by: Some((0, true)), limit: Some(10) }"),
            "{src}"
        );
    }

    fn sample_join_query() -> Query {
        Query {
            columns: vec![
                SelectItem::Column("id".into()),
                SelectItem::Column("budget".into()),
            ],
            from: "a".into(),
            joins: vec![Join {
                kind: JoinKind::Inner,
                table: "b".into(),
                left_col: "a.id".into(),
                right_col: "b.id".into(),
            }],
            where_clause: None,
            distinct: false,
            group_by: vec![],
            having: None,
            order_by: None,
            limit: None,
            offset: None,
        }
    }

    #[test]
    fn render_joined_reconstructs_query_literal_and_calls_execute_joined() {
        let query = sample_join_query();
        let src = render_joined(
            "column_rs",
            "SELECT a.id, b.budget FROM a JOIN b ON a.id = b.id",
            &query,
        );
        assert!(src.contains("fn build_query() -> Query"), "{src}");
        assert!(
            src.contains("column_rs::query::execute_joined(&main_file, &other_file, &query)"),
            "{src}"
        );
        assert!(src.contains("const MAIN_TABLE: &str = \"a\";"), "{src}");
        assert!(src.contains("const OTHER_TABLE: &str = \"b\";"), "{src}");
        assert!(src.contains("JoinKind::Inner"), "{src}");
    }

    #[test]
    fn render_semi_join_calls_execute_semi_join() {
        let query = Query {
            columns: vec![SelectItem::Column("id".into())],
            from: "orders".into(),
            joins: vec![],
            where_clause: Some(Expr::InSubquery {
                expr: Box::new(Expr::Column("region_key".into())),
                subquery: Box::new(Query {
                    columns: vec![SelectItem::Column("rkey".into())],
                    from: "regions".into(),
                    joins: vec![],
                    where_clause: None,
                    distinct: false,
                    group_by: vec![],
                    having: None,
                    order_by: None,
                    limit: None,
                    offset: None,
                }),
            }),
            distinct: false,
            group_by: vec![],
            having: None,
            order_by: None,
            limit: None,
            offset: None,
        };
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
        assert!(src.contains("Expr::InSubquery"), "{src}");
    }

    #[test]
    fn render_windowed_reconstructs_query_literal_and_calls_execute_windowed() {
        let query = Query {
            columns: vec![SelectItem::Window(WindowSpec {
                func: WindowFunc::RowNumber,
                arg: None,
                offset: None,
                partition_by: vec![],
                order_by: vec![("id".into(), false)],
            })],
            from: "t".into(),
            joins: vec![],
            where_clause: None,
            distinct: false,
            group_by: vec![],
            having: None,
            order_by: None,
            limit: None,
            offset: None,
        };
        let src = render_windowed(
            "column_rs",
            "SELECT ROW_NUMBER() OVER (ORDER BY id) FROM t",
            &query,
        );
        assert!(src.contains("fn build_query() -> Query"), "{src}");
        assert!(
            src.contains("column_rs::query::execute_windowed(&file, &query)"),
            "{src}"
        );
        assert!(src.contains("WindowFunc::RowNumber"), "{src}");
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
            Opcode::Finalize {
                agg_parts: vec![AggPart::Avg(1, 2)].into(),
                num_group_keys: 0,
                distinct: false,
                order_by: None,
                limit: None,
            },
        ] {
            let rendered = render_opcode(&op);
            assert!(!rendered.is_empty());
        }
    }

    #[test]
    #[should_panic(expected = "no planner feeding emit::batch emits")]
    fn render_opcode_panics_on_unplanned_join_opcodes() {
        let _ = render_opcode(&Opcode::HashBuild {
            key_cols: vec![0].into(),
            payload_cols: vec![1].into(),
            table: 0,
        });
    }
}
