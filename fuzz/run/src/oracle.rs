//! The sqlite3 oracle: a persistent `sqlite3` shell per lane, fed one
//! statement at a time over stdin, with a sentinel SELECT after each so
//! the reader knows where one statement's output ends.
//!
//! Why the CLI and not `rusqlite`: the fuzz crates carry no third-party
//! dependencies, and a `findings.jsonl` repro is meant to be replayed by
//! hand with `sqlite3 fixture.db < x.sql` -- so the oracle *is* that
//! command. Why persistent: `BEGIN`/`COMMIT`/`ROLLBACK` are in the
//! generated grammar, and a fresh process per statement would lose the
//! transaction state db-core's engine keeps across the stream.
//!
//! stderr is merged into stdout through a `sh -c 'exec ... 2>&1'` wrapper
//! so an error line arrives in order with the rows, before the sentinel;
//! two pipes would race.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use crate::runner::RunError;

/// The parse.y / fixture pin (`tests/unit/eqp_test.rs`, grammar.ebnf).
pub const EXPECTED_SQLITE_VERSION: &str = "3.53.4";

const SENTINEL: &str = "__db_core_fuzz_sentinel_7f3a__";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OracleOutcome {
    /// Statement ran; rows in `.mode quote` rendering, one `Vec` per row.
    Rows(Vec<Vec<String>>),
    /// sqlite3 reported an error (its message, first line).
    Error(String),
}

#[derive(Debug, Clone)]
pub struct OracleConfig {
    /// `sqlite3` binary (`ORACLE_BIN`).
    pub bin: String,
    /// Accept a version other than [`EXPECTED_SQLITE_VERSION`].
    pub allow_version_mismatch: bool,
}

/// Why an `exec` did not produce an outcome.
#[derive(Debug)]
pub enum OracleFailure {
    /// No sentinel within the timeout; the child has been killed and the
    /// oracle must be respawned and its state replayed.
    Hang,
    /// Pipe/process failure.
    Broken(String),
}

pub struct Sqlite3Cli {
    child: Child,
    stdin: std::process::ChildStdin,
    lines: Receiver<String>,
    version: String,
    timeout: Duration,
}

impl Sqlite3Cli {
    /// Spawns `bin` on `db`, sets quote mode, and reads the version.
    pub fn spawn(config: &OracleConfig, db: &Path, timeout: Duration) -> Result<Self, RunError> {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("exec \"$0\" -batch \"$1\" 2>&1")
            .arg(&config.bin)
            .arg(db)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| RunError(format!("spawning oracle {}: {e}", config.bin)))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| RunError("oracle stdin unavailable".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| RunError("oracle stdout unavailable".to_string()))?;
        let (tx, lines) = mpsc::channel::<String>();
        thread::Builder::new()
            .name("fuzz-oracle-reader".to_string())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    match line {
                        Ok(l) => {
                            if tx.send(l).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
            .map_err(|e| RunError(format!("spawning oracle reader: {e}")))?;
        let mut cli = Sqlite3Cli {
            child,
            stdin,
            lines,
            version: String::new(),
            timeout,
        };
        cli.write(".bail off\n.mode quote\n.headers off\n")?;
        let version = match cli.exec("SELECT sqlite_version()") {
            Ok(OracleOutcome::Rows(rows)) => rows
                .first()
                .and_then(|r| r.first())
                .map(|v| v.trim_matches('\'').to_string())
                .unwrap_or_default(),
            Ok(OracleOutcome::Error(e)) => {
                return Err(RunError(format!("oracle version probe failed: {e}")))
            }
            Err(f) => return Err(RunError(format!("oracle version probe: {f:?}"))),
        };
        if version != EXPECTED_SQLITE_VERSION && !config.allow_version_mismatch {
            return Err(RunError(format!(
                "oracle {} is sqlite {version}, expected {EXPECTED_SQLITE_VERSION}; point ORACLE_BIN at a {EXPECTED_SQLITE_VERSION} binary (e.g. /opt/homebrew/opt/sqlite/bin/sqlite3) or set ALLOW_ORACLE_VERSION=1",
                config.bin
            )));
        }
        cli.version = version;
        Ok(cli)
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    fn write(&mut self, text: &str) -> Result<(), RunError> {
        self.stdin
            .write_all(text.as_bytes())
            .and_then(|()| self.stdin.flush())
            .map_err(|e| RunError(format!("writing to oracle: {e}")))
    }

    /// Runs one statement and collects its output up to the sentinel.
    pub fn exec(&mut self, sql: &str) -> Result<OracleOutcome, OracleFailure> {
        let stmt = sql.trim().trim_end_matches(';');
        self.write(&format!("{stmt};\nSELECT '{SENTINEL}';\n"))
            .map_err(|e| OracleFailure::Broken(e.0))?;
        let sentinel_line = format!("'{SENTINEL}'");
        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut error: Option<String> = None;
        loop {
            match self.lines.recv_timeout(self.timeout) {
                Ok(line) if line == sentinel_line => break,
                Ok(line) => {
                    if is_error_line(&line) {
                        if error.is_none() {
                            error = Some(line);
                        }
                    } else if error.is_none() {
                        rows.push(split_quote_row(&line));
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.child.kill().ok();
                    return Err(OracleFailure::Hang);
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(OracleFailure::Broken("oracle exited".to_string()));
                }
            }
        }
        Ok(match error {
            Some(e) => OracleOutcome::Error(e),
            None => OracleOutcome::Rows(rows),
        })
    }
}

impl Drop for Sqlite3Cli {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

fn is_error_line(line: &str) -> bool {
    line.starts_with("Parse error")
        || line.starts_with("Runtime error")
        || line.starts_with("Error:")
        || line.starts_with("Error near")
}

/// Splits one `.mode quote` output line into cells. Cells are `NULL`,
/// numbers, `'text'` with `''` escapes, or `X'hex'`; separated by `,`.
/// Blob hex is lower-cased so both sides render alike.
pub fn split_quote_row(line: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                cur.push('\'');
                if in_quote {
                    if chars.peek() == Some(&'\'') {
                        cur.push('\'');
                        chars.next();
                    } else {
                        in_quote = false;
                    }
                } else {
                    in_quote = true;
                }
            }
            ',' if !in_quote => {
                cells.push(std::mem::take(&mut cur));
            }
            c => cur.push(c),
        }
    }
    cells.push(cur);
    cells
        .into_iter()
        .map(|c| {
            if c.starts_with("X'") || c.starts_with("x'") {
                format!("X{}", c.get(1..).unwrap_or("").to_ascii_lowercase())
            } else {
                c
            }
        })
        .collect()
}
