//! The three ROW-frontend stages and how one statement's outcome at each
//! is classified.

use db_core::codegen::row::leading_keywords;
use db_core::engine::row::RowEngine;
use db_core::engine::{Engine, EngineError, ErrorKind};
use db_core::parser::row::{
    parse_analyze, parse_begin, parse_commit, parse_create_index, parse_create_table,
    parse_create_view, parse_delete, parse_drop_index, parse_drop_table, parse_drop_view,
    parse_explain, parse_insert, parse_pragma, parse_rollback, parse_select, parse_update,
    ParseOutcome,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Stage {
    /// `parser::row` -- tokenizer + recursive-descent parser.
    Parse,
    /// `codegen::row` -- name resolution and program emission (probed
    /// via the engine's compile-only explain path).
    Codegen,
    /// `vm::row` -- program execution against the pager.
    Vm,
}

impl Stage {
    /// Parses the `STAGE=` knob value.
    pub fn parse(raw: &str) -> Option<Stage> {
        match raw {
            "parse" => Some(Stage::Parse),
            "codegen" => Some(Stage::Codegen),
            "vm" => Some(Stage::Vm),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Parse => "parse",
            Stage::Codegen => "codegen",
            Stage::Vm => "vm",
        }
    }
}

/// A typed, expected refusal at some stage. Never a finding.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Rejection {
    /// Parser said the input is not valid SQL.
    Invalid,
    /// Parser or codegen said the input is valid SQL db-core does not
    /// implement yet (V-block not landed). Tracked separately so a run
    /// can report how much of the grammar is still ahead of the engine.
    Unsupported,
    /// Codegen refused (unknown table/column, type mismatch, ...).
    Compile,
    /// VM refused (constraint violation, no such rowid, ...).
    Execute,
}

impl Rejection {
    pub fn as_str(&self) -> &'static str {
        match self {
            Rejection::Invalid => "invalid",
            Rejection::Unsupported => "unsupported",
            Rejection::Compile => "compile",
            Rejection::Execute => "execute",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Ran through all three stages and produced a result.
    Ok,
    /// Stopped at `stage` with a typed error. Expected; counted only.
    Rejected {
        stage: Stage,
        kind: Rejection,
        message: String,
    },
    /// A stage panicked (class 4 finding).
    Panic { stage: Stage, message: String },
    /// A stage exceeded the per-statement timeout (class 4 finding).
    Hang { stage: Stage },
    /// The statement returned normally from the VM but left the database
    /// unreadable: the catalog no longer parses (class 4 finding -- a
    /// write that silently destroys the file is as total a failure as a
    /// panic). `script` is every statement that reached the VM on this
    /// engine since it was opened, in order: a self-contained repro.
    Corrupted {
        message: String,
        script: Vec<String>,
    },
}

impl Outcome {
    pub fn is_finding(&self) -> bool {
        matches!(
            self,
            Outcome::Panic { .. } | Outcome::Hang { .. } | Outcome::Corrupted { .. }
        )
    }
}

/// Stage 1: dispatch to the per-statement parser entry points the same
/// way `codegen::row::dispatch::compile_statement` does, but without
/// compiling -- so a panic here is the parser's alone.
///
/// Returns `Ok(())` when the statement parsed, or the rejection.
pub fn parse_stage(sql: &str) -> Result<(), (Rejection, String)> {
    let kws = leading_keywords(sql);
    let first = kws.first().map(String::as_str).unwrap_or("");
    let second = kws.get(1).map(String::as_str).unwrap_or("");
    let third = kws.get(2).map(String::as_str).unwrap_or("");

    fn done<T>(outcome: ParseOutcome<T>) -> Result<(), (Rejection, String)> {
        match outcome {
            ParseOutcome::Accepted(_) => Ok(()),
            ParseOutcome::Unsupported { message, span } => Err((
                Rejection::Unsupported,
                format!("{message} (line {}, column {})", span.line, span.column),
            )),
            ParseOutcome::Invalid { message, span } => Err((
                Rejection::Invalid,
                format!("{message} (line {}, column {})", span.line, span.column),
            )),
        }
    }

    match first {
        "SELECT" | "WITH" => done(parse_select(sql)),
        "INSERT" | "REPLACE" => done(parse_insert(sql)),
        "UPDATE" => done(parse_update(sql)),
        "DELETE" => done(parse_delete(sql)),
        "EXPLAIN" => done(parse_explain(sql)),
        "PRAGMA" => done(parse_pragma(sql)),
        "ANALYZE" => done(parse_analyze(sql)),
        "BEGIN" => done(parse_begin(sql)),
        "COMMIT" | "END" => done(parse_commit(sql)),
        "ROLLBACK" => done(parse_rollback(sql)),
        "CREATE" => match (second, third) {
            ("INDEX", _) | ("UNIQUE", _) => done(parse_create_index(sql)),
            ("VIEW", _) | ("TEMP", "VIEW") | ("TEMPORARY", "VIEW") => done(parse_create_view(sql)),
            _ => done(parse_create_table(sql)),
        },
        "DROP" => match second {
            "INDEX" => done(parse_drop_index(sql)),
            "VIEW" => done(parse_drop_view(sql)),
            _ => done(parse_drop_table(sql)),
        },
        other => Err((
            Rejection::Invalid,
            format!("no row parser entry point for leading keyword '{other}'"),
        )),
    }
}

fn classify_engine_error(e: &EngineError) -> (Rejection, String) {
    let kind = match e.kind {
        ErrorKind::Parse => Rejection::Invalid,
        ErrorKind::Unsupported => Rejection::Unsupported,
        ErrorKind::Compile | ErrorKind::Open => Rejection::Compile,
        ErrorKind::Execute => Rejection::Execute,
    };
    (kind, e.message.clone())
}

/// Stage 2: compile without executing. `Engine::explain_opcodes` parses
/// and compiles against the live catalog and renders the program, but
/// never runs it. Stage 1 already proved the parser returns normally on
/// this input, so a panic here belongs to codegen.
pub fn codegen_stage(engine: &RowEngine, sql: &str) -> Result<(), (Rejection, String)> {
    engine
        .explain_opcodes(sql)
        .map(|_| ())
        .map_err(|e| classify_engine_error(&e))
}

/// Stage 3: execute for real. Stages 1-2 already returned normally, so a
/// panic here belongs to the VM (or the storage layer beneath it).
pub fn vm_stage(engine: &mut RowEngine, sql: &str) -> Result<(), (Rejection, String)> {
    engine
        .run_query(sql)
        .map(|_| ())
        .map_err(|e| classify_engine_error(&e))
}
