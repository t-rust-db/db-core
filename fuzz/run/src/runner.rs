//! Worker-thread execution with panic capture and a per-statement
//! timeout.
//!
//! `RowEngine` holds `Rc<RefCell<Pager>>`, so it is neither `Send` nor
//! `UnwindSafe`. The engine therefore lives *inside* a worker thread that
//! receives statements over a channel and reports back per stage; the
//! driver waits with `recv_timeout`. A panic is caught in the worker
//! (`AssertUnwindSafe` -- the engine is discarded afterwards, so its
//! poisoned state never gets reused). A hang cannot be interrupted: the
//! worker is abandoned (the thread leaks and keeps its CPU until the
//! process exits -- acceptable for a bounded fuzz run, and the price of
//! not needing a subprocess per statement) and a fresh worker with a
//! fresh fixture copy takes over.

use std::collections::BTreeMap;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use db_core::engine::row::RowEngine;
use db_core::engine::{Cell, Engine, TableInfo};

use crate::findings::{Finding, FindingsSink};
use crate::stage::{codegen_stage, parse_stage, vm_stage, Outcome, Rejection, Stage};

#[derive(Debug)]
pub struct RunError(pub String);

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "run error: {}", self.0)
    }
}

impl std::error::Error for RunError {}

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub fixture: PathBuf,
    pub timeout: Duration,
    /// When false, statements touching implementation-defined corners
    /// (`RANDOM()`, `CURRENT_*`) are skipped rather than run, so a later
    /// oracle comparison never sees them. Totality alone would not need
    /// this; it is here so runs are reproducible row-for-row (#546).
    pub allow_impldef: bool,
    /// Last probe to run. `Stage::Vm` (the default) runs the whole chain;
    /// `Stage::Parse` fuzzes the parser alone, `Stage::Codegen` stops
    /// after compile-only. A statement that clears the last requested
    /// stage counts as `Ok`.
    pub stop_after: Stage,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RunSummary {
    pub total: usize,
    pub ok: usize,
    pub skipped_impldef: usize,
    /// `(stage, rejection)` -> count.
    pub rejected: BTreeMap<(Stage, Rejection), usize>,
    pub findings: Vec<Finding>,
}

impl RunSummary {
    pub fn render(&self) -> String {
        let mut s = format!(
            "statements: {}  ok: {}  skipped(impldef): {}  findings: {}\n",
            self.total,
            self.ok,
            self.skipped_impldef,
            self.findings.len()
        );
        for ((stage, kind), n) in &self.rejected {
            s.push_str(&format!(
                "  rejected at {:<7} {:<11} {n}\n",
                stage.as_str(),
                kind.as_str()
            ));
        }
        for f in &self.findings {
            s.push_str(&format!(
                "  FINDING {} in {} [seed {} #{}]{}: {}\n",
                f.class,
                f.stage.as_str(),
                f.seed,
                f.index,
                if f.script.is_empty() {
                    String::new()
                } else {
                    format!(" (after {} statements)", f.script.len())
                },
                f.sql
            ));
        }
        s
    }
}

// ---------------------------------------------------------------------
// Panic capture

static LAST_PANIC: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn last_panic() -> &'static Mutex<Option<String>> {
    LAST_PANIC.get_or_init(|| Mutex::new(None))
}

thread_local! {
    /// True only while this thread is inside [`probe`]'s `catch_unwind`;
    /// panics anywhere else (test assertions, the driver itself) go to
    /// the previously installed hook and print as usual.
    static CAPTURING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Installs (once per process) a panic hook that, while a stage probe is
/// running on the panicking thread, records the payload and location
/// instead of printing them -- so a fuzz run's stderr stays readable and
/// the message reaches the finding record. Outside a probe the previous
/// hook runs unchanged.
pub fn install_panic_capture() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if !CAPTURING.with(std::cell::Cell::get) {
                previous(info);
                return;
            }
            let msg = info
                .payload()
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic payload>".to_string());
            let loc = info
                .location()
                .map(|l| format!(" at {}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_default();
            if let Ok(mut slot) = last_panic().lock() {
                *slot = Some(format!("{msg}{loc}"));
            }
        }));
    });
}

fn take_panic_message() -> String {
    last_panic()
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
        .unwrap_or_else(|| "<panic message unavailable>".to_string())
}

// ---------------------------------------------------------------------
// Temp fixture copies

#[derive(Debug)]
pub struct TempDb(PathBuf);

impl TempDb {
    pub fn copy_of(fixture: &Path, label: &str) -> Result<Self, RunError> {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "db-core-fuzz-{label}-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::copy(fixture, &path)
            .map_err(|e| RunError(format!("copying fixture {}: {e}", fixture.display())))?;
        Ok(TempDb(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
        std::fs::remove_file(format!("{}-journal", self.0.display())).ok();
    }
}

/// Opens a scratch copy of `fixture` and lists its tables -- what the
/// generator's dialect table is seeded from, so it can never drift from
/// what the workers actually run against.
pub fn catalog_of(fixture: &Path) -> Result<Vec<TableInfo>, RunError> {
    let db = TempDb::copy_of(fixture, "catalog")?;
    let engine = RowEngine::open(db.path())
        .map_err(|e| RunError(format!("opening {}: {}", db.path().display(), e.message)))?;
    engine
        .tables()
        .map_err(|e| RunError(format!("listing tables: {}", e.message)))
}

// ---------------------------------------------------------------------
// Worker

enum Progress {
    Entering(Stage),
    Done(Outcome),
}

struct Worker {
    jobs: Sender<String>,
    progress: Receiver<Progress>,
}

impl Worker {
    fn spawn(fixture: PathBuf, label: usize, stop_after: Stage) -> Result<Self, RunError> {
        let (jobs, job_rx) = mpsc::channel::<String>();
        let (progress_tx, progress) = mpsc::channel::<Progress>();
        // The engine is `!Send`, so it is opened inside the thread; the
        // first message back reports whether that worked.
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), RunError>>();
        thread::Builder::new()
            .name(format!("fuzz-worker-{label}"))
            .spawn(move || {
                let opened = TempDb::copy_of(&fixture, &format!("w{label}")).and_then(|db| {
                    match RowEngine::open(db.path()) {
                        Ok(e) => Ok((db, e)),
                        Err(e) => Err(RunError(format!(
                            "opening {}: {}",
                            db.path().display(),
                            e.message
                        ))),
                    }
                });
                let (db, mut engine) = match opened {
                    Ok(pair) => pair,
                    Err(e) => {
                        ready_tx.send(Err(e)).ok();
                        return;
                    }
                };
                if ready_tx.send(Ok(())).is_err() {
                    return;
                }
                // `db` must outlive the engine: keep it in the closure.
                let _db = &db;
                // Every statement that reached the VM on the current
                // engine, for corruption repro scripts.
                let mut history: Vec<String> = Vec::new();
                while let Ok(sql) = job_rx.recv() {
                    let mut outcome =
                        run_stages(&mut engine, &sql, &progress_tx, &mut history, stop_after);
                    if stop_after == Stage::Vm
                        && matches!(
                            outcome,
                            Outcome::Ok
                                | Outcome::Rejected {
                                    stage: Stage::Vm,
                                    ..
                                }
                        )
                    {
                        if let Err(message) = health_check(&mut engine) {
                            outcome = Outcome::Corrupted {
                                message,
                                script: history.clone(),
                            };
                        }
                    }
                    if matches!(outcome, Outcome::Panic { .. } | Outcome::Corrupted { .. }) {
                        history.clear();
                        // State after a caught panic is suspect: start
                        // over from a clean fixture copy.
                        match TempDb::copy_of(&fixture, &format!("w{label}r")).and_then(|fresh| {
                            RowEngine::open(fresh.path())
                                .map(|e| (fresh, e))
                                .map_err(|e| RunError(e.message))
                        }) {
                            Ok((fresh, fresh_engine)) => {
                                // Leak the old TempDb deliberately until
                                // thread exit; the new one is what we run on.
                                std::mem::forget(fresh);
                                engine = fresh_engine;
                            }
                            Err(_) => break,
                        }
                    }
                    if progress_tx.send(Progress::Done(outcome)).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| RunError(format!("spawning worker: {e}")))?;
        ready_rx
            .recv()
            .map_err(|_| RunError("worker exited before opening its fixture".to_string()))??;
        Ok(Worker { jobs, progress })
    }
}

/// Post-statement health probe: the catalog must still read and
/// `PRAGMA quick_check` must report `ok`. The quick check costs well
/// under a millisecond on the fixture and catches *latent* damage (a
/// page both free and in use) at the statement that caused it, where a
/// bare catalog read only fails once something reuses the page.
fn health_check(engine: &mut RowEngine) -> Result<(), String> {
    engine.tables().map_err(|e| e.message)?;
    let report = engine
        .run_query("PRAGMA quick_check")
        .map_err(|e| format!("quick_check failed: {}", e.message))?;
    let lines: Vec<String> = report
        .rows
        .iter()
        .flat_map(|r| r.iter())
        .map(|c| match c {
            Cell::Text(t) => t.clone(),
            other => format!("{other:?}"),
        })
        .collect();
    if lines.len() == 1 && lines.first().is_some_and(|l| l == "ok") {
        Ok(())
    } else {
        Err(format!("quick_check: {}", lines.join(" | ")))
    }
}

/// Runs one stage's closure under `catch_unwind`, mapping a panic to
/// `Outcome::Panic` and a typed refusal to `Outcome::Rejected`; `Ok(())`
/// means proceed to the next stage.
pub fn probe<F>(stage: Stage, f: F) -> Result<(), Outcome>
where
    F: FnOnce() -> Result<(), (Rejection, String)>,
{
    CAPTURING.with(|c| c.set(true));
    let caught = panic::catch_unwind(AssertUnwindSafe(f));
    CAPTURING.with(|c| c.set(false));
    match caught {
        Err(_) => Err(Outcome::Panic {
            stage,
            message: take_panic_message(),
        }),
        Ok(Err((kind, message))) => Err(Outcome::Rejected {
            stage,
            kind,
            message,
        }),
        Ok(Ok(())) => Ok(()),
    }
}

fn run_stages(
    engine: &mut RowEngine,
    sql: &str,
    progress: &Sender<Progress>,
    history: &mut Vec<String>,
    stop_after: Stage,
) -> Outcome {
    progress.send(Progress::Entering(Stage::Parse)).ok();
    if let Err(outcome) = probe(Stage::Parse, || parse_stage(sql)) {
        return outcome;
    }
    if stop_after == Stage::Parse {
        return Outcome::Ok;
    }
    progress.send(Progress::Entering(Stage::Codegen)).ok();
    if let Err(outcome) = probe(Stage::Codegen, || codegen_stage(engine, sql)) {
        return outcome;
    }
    if stop_after == Stage::Codegen {
        return Outcome::Ok;
    }
    progress.send(Progress::Entering(Stage::Vm)).ok();
    history.push(sql.to_string());
    match probe(Stage::Vm, || vm_stage(engine, sql)) {
        Err(outcome) => outcome,
        Ok(()) => Outcome::Ok,
    }
}

// ---------------------------------------------------------------------
// Driver

pub struct Runner {
    config: RunConfig,
    worker: Worker,
    spawned: usize,
}

impl Runner {
    pub fn new(config: RunConfig) -> Result<Self, RunError> {
        install_panic_capture();
        let worker = Worker::spawn(config.fixture.clone(), 0, config.stop_after)?;
        Ok(Runner {
            config,
            worker,
            spawned: 1,
        })
    }

    /// Runs one statement through all stages; on a hang, abandons the
    /// worker and starts a fresh one before returning.
    pub fn run_one(&mut self, sql: &str) -> Result<Outcome, RunError> {
        if self.worker.jobs.send(sql.to_string()).is_err() {
            self.respawn()?;
            self.worker
                .jobs
                .send(sql.to_string())
                .map_err(|e| RunError(format!("worker unreachable: {e}")))?;
        }
        let started = Instant::now();
        let mut stage = Stage::Parse;
        loop {
            let remaining = self.config.timeout.saturating_sub(started.elapsed());
            match self.worker.progress.recv_timeout(remaining) {
                Ok(Progress::Entering(s)) => stage = s,
                Ok(Progress::Done(outcome)) => return Ok(outcome),
                Err(RecvTimeoutError::Timeout) => {
                    self.respawn()?;
                    return Ok(Outcome::Hang { stage });
                }
                Err(RecvTimeoutError::Disconnected) => {
                    // Worker died without reporting (e.g. failed to
                    // re-open its fixture after a panic).
                    self.respawn()?;
                    return Err(RunError(
                        "worker exited without reporting an outcome".to_string(),
                    ));
                }
            }
        }
    }

    fn respawn(&mut self) -> Result<(), RunError> {
        self.worker = Worker::spawn(
            self.config.fixture.clone(),
            self.spawned,
            self.config.stop_after,
        )?;
        self.spawned = self.spawned.saturating_add(1);
        Ok(())
    }

    /// Number of worker threads started so far (1 + one per hang/death).
    pub fn workers_spawned(&self) -> usize {
        self.spawned
    }

    pub fn is_impldef(sql: &str) -> bool {
        let upper = sql.to_ascii_uppercase();
        upper.contains("RANDOM") || upper.contains("CURRENT_")
    }

    /// Runs `statements` (each tagged with its generator index),
    /// recording findings to `sink` when given.
    pub fn run_all<I>(
        &mut self,
        seed: u64,
        statements: I,
        mut sink: Option<&mut FindingsSink>,
    ) -> Result<RunSummary, RunError>
    where
        I: IntoIterator<Item = (usize, String)>,
    {
        let mut summary = RunSummary::default();
        for (index, sql) in statements {
            summary.total = summary.total.saturating_add(1);
            if !self.config.allow_impldef && Self::is_impldef(&sql) {
                summary.skipped_impldef = summary.skipped_impldef.saturating_add(1);
                continue;
            }
            let started = Instant::now();
            let outcome = self.run_one(&sql)?;
            let elapsed_ms = started.elapsed().as_millis();
            match &outcome {
                Outcome::Ok => summary.ok = summary.ok.saturating_add(1),
                Outcome::Rejected { stage, kind, .. } => {
                    let n = summary.rejected.entry((*stage, kind.clone())).or_insert(0);
                    *n = n.saturating_add(1);
                }
                Outcome::Panic { .. } | Outcome::Hang { .. } | Outcome::Corrupted { .. } => {}
            }
            if let Some(f) = Finding::from_outcome(seed, index, &sql, &outcome, elapsed_ms) {
                if let Some(sink) = sink.as_deref_mut() {
                    sink.record(&f)
                        .map_err(|e| RunError(format!("writing finding: {e}")))?;
                }
                summary.findings.push(f);
            }
        }
        Ok(summary)
    }
}
