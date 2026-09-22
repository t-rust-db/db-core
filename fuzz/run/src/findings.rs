//! Class-4 finding records: `findings.jsonl` (one object per line) plus a
//! minimal `<seed>-<index>.sql` per finding so a hit can be replayed
//! without the generator (`sqlite3 fixture.db < x.sql`, or `REPLAY=`).
//!
//! Hand-rolled JSON: the record is flat and small, and the fuzz crates
//! carry no third-party dependencies (fuzz-gen has none either).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::compare::{render_diff, Comparison};
use crate::stage::{Outcome, Stage};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub seed: u64,
    pub index: usize,
    pub stage: Stage,
    /// `"panic"`, `"hang"` or `"corruption"`.
    pub class: &'static str,
    pub message: String,
    pub sql: String,
    /// For `corruption`: every VM-reaching statement on that engine
    /// before and including `sql`, so the `.sql` file replays the whole
    /// history. Empty otherwise.
    pub script: Vec<String>,
    pub elapsed_ms: u128,
    /// Which of `jobs` parallel lanes ran the statement (0-based). With
    /// `JOBS=1` always `0`/`1`. Statements are dealt round-robin, so a
    /// lane's engine history depends on `jobs`; a corruption repro is
    /// self-contained via `script`, but re-running the whole batch
    /// needs the same `JOBS`.
    pub lane: usize,
    pub jobs: usize,
    /// For oracle verdicts: the rendered ours-vs-oracle diff. Empty
    /// otherwise.
    pub detail: String,
    /// ddmin-reduced `script`, when the reducer ran. Empty otherwise.
    pub reduced: Vec<String>,
}

impl Finding {
    /// `None` for outcomes that are not findings.
    pub fn from_outcome(
        seed: u64,
        index: usize,
        sql: &str,
        outcome: &Outcome,
        elapsed_ms: u128,
    ) -> Option<Self> {
        let (stage, class, message, script) = match outcome {
            Outcome::Panic { stage, message } => (*stage, "panic", message.clone(), Vec::new()),
            Outcome::Hang { stage } => (*stage, "hang", String::new(), Vec::new()),
            Outcome::Corrupted { message, script } => {
                (Stage::Vm, "corruption", message.clone(), script.clone())
            }
            Outcome::Ok | Outcome::Rejected { .. } => return None,
        };
        Some(Finding {
            seed,
            index,
            stage,
            class,
            message,
            sql: sql.to_string(),
            script,
            elapsed_ms,
            lane: 0,
            jobs: 1,
            detail: String::new(),
            reduced: Vec::new(),
        })
    }

    /// A differential finding (`wrong-answer`, `gap`, `over-permissive`,
    /// `oracle-hang`). `script` is left empty here; the worker's applied
    /// history is attached by the caller when it has it.
    pub fn from_comparison(
        seed: u64,
        index: usize,
        sql: &str,
        cmp: &Comparison,
        elapsed_ms: u128,
    ) -> Self {
        Finding {
            seed,
            index,
            stage: Stage::Vm,
            class: cmp.verdict.as_str(),
            message: cmp
                .oracle_message
                .clone()
                .or_else(|| cmp.ours_message.clone())
                .unwrap_or_default(),
            sql: sql.to_string(),
            script: cmp.script.clone(),
            elapsed_ms,
            lane: 0,
            jobs: 1,
            detail: render_diff(cmp, 8),
            reduced: Vec::new(),
        }
    }

    pub fn to_json_line(&self) -> String {
        format!(
            "{{\"seed\":{},\"index\":{},\"stage\":\"{}\",\"class\":\"{}\",\"message\":\"{}\",\"sql\":\"{}\",\"script_len\":{},\"elapsed_ms\":{},\"lane\":{},\"jobs\":{},\"reduced_len\":{},\"detail\":\"{}\"}}",
            self.seed,
            self.index,
            self.stage.as_str(),
            self.class,
            json_escape(&self.message),
            json_escape(&self.sql),
            self.script.len(),
            self.elapsed_ms,
            self.lane,
            self.jobs,
            self.reduced.len(),
            json_escape(&self.detail)
        )
    }
}

pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Appends findings under `out_dir`.
#[derive(Debug)]
pub struct FindingsSink {
    out_dir: PathBuf,
    jsonl: File,
}

impl FindingsSink {
    pub fn open(out_dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(out_dir)?;
        let jsonl = OpenOptions::new()
            .create(true)
            .append(true)
            .open(out_dir.join("findings.jsonl"))?;
        Ok(FindingsSink {
            out_dir: out_dir.to_path_buf(),
            jsonl,
        })
    }

    /// Writes `report.md` next to `findings.jsonl` (overwritten per run).
    pub fn write_report(&self, text: &str) -> io::Result<PathBuf> {
        let path = self.out_dir.join("report.md");
        fs::write(&path, text)?;
        Ok(path)
    }

    pub fn record(&mut self, finding: &Finding) -> io::Result<PathBuf> {
        writeln!(self.jsonl, "{}", finding.to_json_line())?;
        self.write_sql(finding)
    }

    /// Rewrites only the `.sql` file (after reduction); the `.jsonl`
    /// line already written stays as is.
    pub fn write_sql(&self, finding: &Finding) -> io::Result<PathBuf> {
        let sql_path = self
            .out_dir
            .join(format!("{}-{}.sql", finding.seed, finding.index));
        let body = if !finding.reduced.is_empty() {
            let mut b = format!(
                "-- ddmin-reduced from {} to {} statements; the last is the trigger\n",
                finding.script.len(),
                finding.reduced.len()
            );
            for s in &finding.reduced {
                b.push_str(s);
                b.push_str(";\n");
            }
            b
        } else if finding.script.is_empty() {
            format!("{};\n", finding.sql)
        } else {
            let mut b =
                String::from("-- full history on this engine; the last statement is the trigger\n");
            for s in &finding.script {
                b.push_str(s);
                b.push_str(";\n");
            }
            b
        };
        fs::write(
            &sql_path,
            format!(
                "-- {} in {} (seed {}, index {})\n{}{}",
                finding.class,
                finding.stage.as_str(),
                finding.seed,
                finding.index,
                if finding.message.is_empty() {
                    String::new()
                } else {
                    format!("-- {}\n", finding.message.replace('\n', "\n-- "))
                } + &if finding.detail.is_empty() {
                    String::new()
                } else {
                    format!("-- {}\n", finding.detail.trim_end().replace('\n', "\n-- "))
                },
                body
            ),
        )?;
        Ok(sql_path)
    }
}
