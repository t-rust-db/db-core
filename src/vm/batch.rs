//! `BatchExecutor`: the vectorized/columnar query VM, one of `sql-vm`'s
//! three executors (see crate root docs) -- extracted from column-rs's
//! private `src/vm.rs`, which was its only consumer, so any engine
//! executing queries in batches over `sql_expr`-compiled programs
//! (column-rs today; loglume potentially later) can depend on this instead
//! of reimplementing it.
//!
//! A small register machine that executes compiled `sql_expr`-compiled
//! queries over batches of column values (default batch size 1024 rows,
//! see [`BATCH_SIZE`]).
//!
//! Each register holds one column's worth of values for the current batch
//! (`Vec<Value>`, one entry per row). Opcodes operate on whole registers at
//! once rather than row-by-row.

use crate::expr::AggFunc;
pub use crate::join::JoinKind;
use crate::join::{should_emit, JoinHashTable};
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::hash::{Hash, Hasher};

/// Rows per batch that opcodes operate on at once.
pub const BATCH_SIZE: usize = 1024;

/// A runtime row value, or a `SELECT`-list literal baked into an
/// [`Opcode::LoadConst`] -- `Str` uses `Cow<'static, str>` so an emitted
/// `const PROGRAM` (see `crate::emit::batch`, #98) can hold `Cow::Borrowed("...")`
/// literals with no heap allocation, while runtime column data (decoded
/// from arbitrary Parquet `BYTE_ARRAY` bytes) still owns its `String` via
/// `Cow::Owned`.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(Cow<'static, str>),
    Null,
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(v) => write!(f, "{v}"),
            Value::Float(v) => write!(f, "{v}"),
            Value::Bool(v) => write!(f, "{v}"),
            Value::Str(v) => write!(f, "{v}"),
            Value::Null => write!(f, "NULL"),
        }
    }
}

impl Value {
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(v) => Some(*v as f64),
            Value::Float(v) => Some(*v),
            _ => None,
        }
    }
}

/// A named batch of columns (one `Vec<Value>` per column, all the same
/// length) — the VM's input for one segment/row-group of a table.
#[derive(Debug, Default, Clone)]
pub struct Batch {
    pub columns: HashMap<String, Vec<Value>>,
    pub num_rows: usize,
}

impl Batch {
    pub fn new(num_rows: usize) -> Self {
        Batch {
            columns: HashMap::new(),
            num_rows,
        }
    }

    pub fn with_column(mut self, name: impl Into<String>, values: Vec<Value>) -> Self {
        self.columns.insert(name.into(), values);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapOp {
    Add,
    Sub,
    Mul,
    Div,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    /// `NOT a` -- unary; `b` is unused (callers pass the same register as
    /// `a`). `NOT NULL` is `NULL`, per the general null-propagation rule
    /// below.
    Not,
    /// `a IS NULL` -- unary; `b` is unused. Unlike every other op, this
    /// does *not* propagate `NULL` -- testing a value for nullness must
    /// itself always produce `true`/`false`.
    IsNull,
    /// `a IS NOT NULL` -- unary; `b` is unused. See [`MapOp::IsNull`].
    IsNotNull,
    /// `a || b` string concatenation (DuckDB/Postgres-style) -- both
    /// operands are stringified via [`Value`]'s `Display` impl rather
    /// than requiring `Str`, so `1 || 'x'` produces `"1x"` instead of
    /// erroring.
    Concat,
    /// `-a` -- unary; `b` is unused, same convention as [`MapOp::Not`].
    /// `Int`/`Float` negate; anything else (including `Null`, per the
    /// general null-propagation rule below) is `Null`.
    Neg,
}

/// Window functions supported by [`Opcode::Window`] -- `Sum`/`Avg`/`Count`
/// here are the running/whole-partition `OVER` forms, distinct from
/// [`AggFunc`]'s flat/`GROUP BY` reductions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowFunc {
    RowNumber,
    Rank,
    DenseRank,
    Lag,
    Lead,
    FirstValue,
    LastValue,
    Sum,
    Avg,
    Count,
}

/// One `GROUP BY`/aggregate part of an emitted row, in emit order -- the
/// per-segment `GroupReduce` output's shape, which [`Opcode::Finalize`]
/// needs to merge partial aggregates across segments (`Sum`/`Count` add,
/// `Min`/`Max` compare, `Avg` divides its `(sum_index, count_index)` pair
/// at the very end). Pure planning metadata, storage-agnostic. `Copy` so
/// an AOT-emitted `const PROGRAM` can hold a `Cow::Borrowed(&[AggPart])`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggPart {
    GroupKey,
    Sum,
    Count,
    Min,
    Max,
    /// `(sum_index, count_index)` into the emitted row, combined at the end.
    Avg(usize, usize),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Opcode {
    /// Load a named column from the current batch into a register.
    LoadColumn {
        reg: usize,
        column: Cow<'static, str>,
    },
    /// Broadcast a constant value to every row of the current batch into a
    /// register.
    LoadConst { reg: usize, value: Value },
    /// Apply a binary op elementwise: `registers[dst] = op(registers[a], registers[b])`.
    Map {
        dst: usize,
        op: MapOp,
        a: usize,
        b: usize,
    },
    /// Keep only the rows where `predicate` register holds `Value::Bool(true)`,
    /// applied to every currently-live register (in place).
    Filter { predicate: usize },
    /// Aggregate a whole register down to a single value (skipping nulls).
    /// `COUNT` counts non-null values, or all rows when `src` is `None`
    /// (`COUNT(*)`).
    Reduce {
        func: AggFunc,
        src: Option<usize>,
        dst: usize,
    },
    /// Hash-aggregate: partition rows by the tuple of values in `group_by`
    /// registers, then reduce each `(func, src)` pair per group. Writes one
    /// row per distinct group into `group_by` registers (deduplicated) plus
    /// one output register per aggregate, in `aggs` order.
    GroupReduce {
        group_by: Cow<'static, [usize]>,
        aggs: Cow<'static, [(AggFunc, Option<usize>)]>,
        agg_dst: Cow<'static, [usize]>,
    },
    /// Build a hash table for an equi-join from the current live registers:
    /// `key_cols` (compound key, NULL-safe -- see [`JoinKey`]) and
    /// `payload_cols` (columns carried through to the probe side), keyed by
    /// `table` so a later [`Opcode::HashProbe`] can find it.
    HashBuild {
        key_cols: Cow<'static, [usize]>,
        payload_cols: Cow<'static, [usize]>,
        table: usize,
    },
    /// Probe the hash table built by the [`Opcode::HashBuild`] that wrote
    /// `table`, keyed by `key_cols` from the current live registers.
    /// Reshapes every currently-live register to the joined row cardinality
    /// (like [`Opcode::Filter`]) and writes the build side's payload columns
    /// into `payload_dst` (NULL-filled for unmatched rows), per `kind`'s
    /// [`should_emit`] rule. `Semi` emits at most one row per probe-side row
    /// regardless of how many build-side rows match, with `payload_dst`
    /// left NULL (semi-joins never surface the build side's columns).
    HashProbe {
        key_cols: Cow<'static, [usize]>,
        table: usize,
        payload_dst: Cow<'static, [usize]>,
        kind: JoinKind,
    },
    /// `func(...) OVER (PARTITION BY ... ORDER BY ...)`: partitions the
    /// current live rows by `partition_by` (empty = one partition), sorts
    /// each partition by `order_by`, computes `func` per row within its
    /// partition, and writes one value per row (in original row order, not
    /// partition/sort order) into `dst`. `arg` is the value column for
    /// `Lag`/`Lead`/`FirstValue`/`LastValue`/`Sum`/`Avg` (unused, `None`,
    /// for `RowNumber`/`Rank`/`DenseRank`, and optional for `Count`, which
    /// counts rows when `arg` is `None`). `offset` is `Lag`/`Lead`'s shift
    /// (default 1). `Sum`/`Avg`/`Count` run as a cumulative aggregate over
    /// `order_by`'s order when `order_by` is non-empty, or once over the
    /// whole partition (broadcast to every row in it) when empty --
    /// matching SQL's default frame for each case.
    Window {
        func: WindowFunc,
        arg: Option<usize>,
        offset: Option<i64>,
        partition_by: Cow<'static, [usize]>,
        order_by: Cow<'static, [(usize, bool)]>,
        dst: usize,
    },
    /// Marks the top of the per-segment loop; a no-op on its own (the
    /// current batch is already loaded by [`Vm::run`]).
    Scan,
    /// Append the current values of `registers` (transposed row-major) to
    /// the VM's output.
    Emit { registers: Cow<'static, [usize]> },
    /// If the source has another segment, load it and jump back to
    /// `loop_start` (the instruction index right after [`Opcode::Scan`]);
    /// otherwise fall through.
    NextSegment { loop_start: usize },
    /// Stop execution.
    Halt,
    /// Terminal opcode of a planned flat program (ADR 0007): the
    /// cross-segment post-processing step -- merge per-segment partial
    /// aggregates by group key (`agg_parts`/`num_group_keys`), then the
    /// final `ORDER BY` (`(output column index, descending)`) and `LIMIT`.
    /// A *barrier*: it needs every segment's output, so the per-segment
    /// [`Vm`] treats it as a no-op control opcode (like [`Opcode::Scan`]/
    /// [`Opcode::Halt`]) and [`crate::vm::engine::run`] applies it once
    /// over the concatenated output. Encodes what used to be sidecar
    /// plan fields so the instruction stream is the whole plan.
    Finalize {
        agg_parts: Cow<'static, [AggPart]>,
        num_group_keys: usize,
        order_by: Option<(usize, bool)>,
        limit: Option<usize>,
    },
}

impl Opcode {
    /// The opcode's variant name, used as `VmError`'s runtime-error context
    /// (the execution-time equivalent of `Span` for parse errors -- there's
    /// no source text left at execution time, but there's always a specific
    /// instruction that failed).
    fn name(&self) -> &'static str {
        match self {
            Opcode::LoadColumn { .. } => "LoadColumn",
            Opcode::LoadConst { .. } => "LoadConst",
            Opcode::Map { .. } => "Map",
            Opcode::Filter { .. } => "Filter",
            Opcode::Reduce { .. } => "Reduce",
            Opcode::GroupReduce { .. } => "GroupReduce",
            Opcode::HashBuild { .. } => "HashBuild",
            Opcode::HashProbe { .. } => "HashProbe",
            Opcode::Window { .. } => "Window",
            Opcode::Scan => "Scan",
            Opcode::Emit { .. } => "Emit",
            Opcode::NextSegment { .. } => "NextSegment",
            Opcode::Halt => "Halt",
            Opcode::Finalize { .. } => "Finalize",
        }
    }
}

/// One instruction of a [`Program`]: a typed [`Opcode`] plus an optional
/// human-readable comment for `EXPLAIN` listings -- the same shape as
/// sqlite-rs's `vdbe::program::Instruction`, except that operands stay
/// typed and named on the `Opcode` enum instead of sqlite-rs's C-heritage
/// `p1..p5` integer slots (ADR 0007: `GroupReduce` alone carries three
/// variable-length slices that don't fit five fixed slots without losing
/// type safety).
#[derive(Debug, Clone, PartialEq)]
pub struct Instruction {
    pub opcode: Opcode,
    pub comment: Option<String>,
}

impl Instruction {
    /// An instruction with no comment.
    pub fn new(opcode: Opcode) -> Self {
        Self {
            opcode,
            comment: None,
        }
    }

    /// An instruction carrying an `EXPLAIN` comment.
    pub fn with_comment(opcode: Opcode, comment: impl Into<String>) -> Self {
        Self {
            opcode,
            comment: Some(comment.into()),
        }
    }
}

/// A linear program of [`Instruction`]s, mirroring sqlite-rs's
/// `vdbe::program::Program`. Everything the executor needs is *in* the
/// instruction stream: the columns to load are the [`Opcode::LoadColumn`]
/// operands, and the cross-segment merge/sort/limit metadata is the
/// trailing [`Opcode::Finalize`] -- no sidecar plan struct (ADR 0007).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Program {
    pub instructions: Vec<Instruction>,
}

impl Program {
    /// Builds a program from its instruction sequence.
    pub fn new(instructions: Vec<Instruction>) -> Self {
        Self { instructions }
    }

    /// Builds a program from bare opcodes (no comments) -- how an AOT
    /// `const PROGRAM: &[Opcode]` re-enters the engine at runtime.
    pub fn from_opcodes<I: IntoIterator<Item = Opcode>>(opcodes: I) -> Self {
        Self {
            instructions: opcodes.into_iter().map(Instruction::new).collect(),
        }
    }

    /// Returns the instruction at `pc`, or `None` if out of range.
    pub fn get(&self, pc: usize) -> Option<&Instruction> {
        self.instructions.get(pc)
    }

    /// The number of instructions in the program.
    pub fn len(&self) -> usize {
        self.instructions.len()
    }

    /// Whether the program has no instructions.
    pub fn is_empty(&self) -> bool {
        self.instructions.is_empty()
    }

    /// The bare opcodes, in order.
    pub fn opcodes(&self) -> impl Iterator<Item = &Opcode> {
        self.instructions.iter().map(|i| &i.opcode)
    }

    /// Every column the program loads, in first-load order -- derived by
    /// scanning for [`Opcode::LoadColumn`] rather than carried separately.
    pub fn columns_to_load(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for op in self.opcodes() {
            if let Opcode::LoadColumn { column, .. } = op {
                if !out.iter().any(|c| c == column.as_ref()) {
                    out.push(column.to_string());
                }
            }
        }
        out
    }

    /// Splits off a trailing [`Opcode::Finalize`]: `(body opcodes, the
    /// Finalize)`. A program without one returns all its opcodes and
    /// `None`, and executes as a plain per-segment concatenation.
    pub fn split_finalize(&self) -> (Vec<Opcode>, Option<&Opcode>) {
        match self.instructions.last() {
            Some(Instruction {
                opcode: fin @ Opcode::Finalize { .. },
                ..
            }) => {
                let n = self.instructions.len() - 1;
                (
                    self.instructions[..n]
                        .iter()
                        .map(|i| i.opcode.clone())
                        .collect(),
                    Some(fin),
                )
            }
            _ => (self.opcodes().cloned().collect(), None),
        }
    }
}

/// Supplies successive batches (row-group segments) of a table to
/// [`Vm::run`].
pub trait Source {
    fn next_batch(&mut self) -> Option<Batch>;
}

/// One independently-loadable unit of work for [`run_parallel`] — typically
/// a single row group's worth of columns.
pub trait Segment: Send + Sync {
    fn load(&self) -> Batch;
}

/// Run `program` (a flat, non-looping instruction list ending in
/// [`Opcode::Emit`]) against every segment in parallel (via rayon,
/// morsel-driven: one segment per task), then concatenate the emitted rows
/// in segment order.
///
/// `GroupReduce`/`Reduce` results are per-segment only — merging partial
/// aggregates across segments is not performed here.
pub fn run_parallel<'s>(
    segments: &[Box<dyn Segment + 's>],
    program: &[Opcode],
) -> Result<Vec<Vec<Value>>> {
    use rayon::prelude::*;

    let per_segment: Vec<Result<Vec<Vec<Value>>>> = segments
        .par_iter()
        .map(|segment| {
            let batch = segment.load();
            let mut vm = Vm::new();
            vm.execute(&batch, program)?;
            Ok(std::mem::take(&mut vm.output))
        })
        .collect();

    let mut all = Vec::new();
    for rows in per_segment {
        all.extend(rows?);
    }
    Ok(all)
}

/// `ORDER BY <col> [ASC|DESC] LIMIT <limit>` spec for [`run_parallel_top_n`]:
/// which emitted output column to order by, direction, and how many rows to
/// keep.
#[derive(Debug, Clone, Copy)]
pub struct TopN {
    pub col: usize,
    pub descending: bool,
    pub limit: usize,
}

/// Ordering for `ORDER BY`: `Null` always sorts after every non-null value,
/// regardless of direction (DuckDB's default `NULLS LAST` for both `ASC`
/// and `DESC`).
pub fn compare_for_order(a: &Value, b: &Value, descending: bool) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (matches!(a, Value::Null), matches!(b, Value::Null)) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => {
            let ord = match (a.as_f64(), b.as_f64()) {
                (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
                _ => a.to_string().cmp(&b.to_string()),
            };
            if descending {
                ord.reverse()
            } else {
                ord
            }
        }
    }
}

/// A row plus enough context (`col`, `descending`) to order it against
/// another for [`top_n_reduce`]'s heap.
struct TopNItem {
    row: Vec<Value>,
    col: usize,
    descending: bool,
}

impl PartialEq for TopNItem {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}
impl Eq for TopNItem {}
impl PartialOrd for TopNItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for TopNItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        compare_for_order(&self.row[self.col], &other.row[self.col], self.descending)
    }
}

/// Bound `rows` down to its top `spec.limit` in `ORDER BY` order via a
/// bounded max-heap of the current worst kept row: `O(rows.len() log
/// spec.limit)` and `O(spec.limit)` peak memory, instead of a full sort's
/// `O(n log n)` time and `O(n)` memory.
fn top_n_reduce(rows: Vec<Vec<Value>>, spec: &TopN) -> Vec<Vec<Value>> {
    use std::collections::BinaryHeap;

    if spec.limit == 0 {
        return Vec::new();
    }

    let mut heap: BinaryHeap<TopNItem> = BinaryHeap::with_capacity(spec.limit.min(rows.len()) + 1);
    for row in rows {
        let item = TopNItem {
            row,
            col: spec.col,
            descending: spec.descending,
        };
        if heap.len() < spec.limit {
            heap.push(item);
        } else if let Some(worst) = heap.peek() {
            if item.cmp(worst) == std::cmp::Ordering::Less {
                heap.pop();
                heap.push(item);
            }
        }
    }
    heap.into_sorted_vec()
        .into_iter()
        .map(|item| item.row)
        .collect()
}

/// Like [`run_parallel`], but for `ORDER BY ... LIMIT ...` queries: each
/// segment is reduced to its own top-`spec.limit` rows before merging, and
/// the merge itself is a final top-`spec.limit` reduction rather than a
/// concatenation -- so peak memory is bounded by `segments.len() *
/// spec.limit` rather than the full row count.
pub fn run_parallel_top_n<'s>(
    segments: &[Box<dyn Segment + 's>],
    program: &[Opcode],
    spec: &TopN,
) -> Result<Vec<Vec<Value>>> {
    use rayon::prelude::*;

    let per_segment: Vec<Result<Vec<Vec<Value>>>> = segments
        .par_iter()
        .map(|segment| {
            let batch = segment.load();
            let mut vm = Vm::new();
            vm.execute(&batch, program)?;
            Ok(top_n_reduce(std::mem::take(&mut vm.output), spec))
        })
        .collect();

    let mut all = Vec::new();
    for rows in per_segment {
        all.extend(rows?);
    }
    Ok(top_n_reduce(all, spec))
}

/// A pathological/buggy compiled program can't run more `Vm::step` calls
/// than this before [`VmError::StepLimitExceeded`] aborts it -- this
/// project's own bounded-execution principle (see `sql-parser`'s `CROSS
/// JOIN` `LIMIT` requirement), applied to VM execution the way sqlite-rs's
/// `ExecError::StepLimitExceeded` bounds its own VDBE loop.
pub const MAX_STEPS: usize = 10_000_000;

/// Errors from executing a compiled program. Every variant that plausibly
/// has one carries `opcode`, the name of the [`Opcode`] variant that
/// triggered it -- the runtime-error equivalent of `Span` for parse errors:
/// there's no source text left at execution time, but there's always a
/// specific instruction that failed.
#[derive(Debug, PartialEq)]
pub enum VmError {
    UnknownColumn {
        opcode: &'static str,
        column: String,
    },
    UnknownRegister {
        opcode: &'static str,
        register: usize,
    },
    RegisterLengthMismatch {
        opcode: &'static str,
    },
    UnknownJoinTable {
        opcode: &'static str,
        table: usize,
    },
    /// `Vm::step` was about to execute past [`MAX_STEPS`] instructions.
    StepLimitExceeded {
        opcode: &'static str,
        limit: usize,
    },
}

impl fmt::Display for VmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VmError::UnknownColumn { opcode, column } => {
                write!(f, "{opcode}: unknown column: {column}")
            }
            VmError::UnknownRegister { opcode, register } => {
                write!(f, "{opcode}: unknown register: {register}")
            }
            VmError::RegisterLengthMismatch { opcode } => {
                write!(f, "{opcode}: register length mismatch")
            }
            VmError::UnknownJoinTable { opcode, table } => {
                write!(f, "{opcode}: unknown join table: {table}")
            }
            VmError::StepLimitExceeded { opcode, limit } => {
                write!(f, "{opcode}: exceeded step limit of {limit}")
            }
        }
    }
}

impl std::error::Error for VmError {}

pub type Result<T> = std::result::Result<T, VmError>;

/// A compound join key built from a probe/build row's key columns.
///
/// NULL-safe by construction: [`PartialEq`] treats any key containing a
/// [`Value::Null`] component as unequal to everything (including another
/// all-`Null` key), matching SQL's `NULL = NULL` is never true rule. `Hash`
/// only needs to agree with `Eq` in one direction (equal keys must hash
/// equal; unequal keys may collide), so hashing every key -- `Null`
/// included -- by its plain contents is sound even though equality itself
/// special-cases `Null`.
#[derive(Debug, Clone)]
struct JoinKey(Vec<Value>);

impl PartialEq for JoinKey {
    fn eq(&self, other: &Self) -> bool {
        if self.0.iter().any(|v| matches!(v, Value::Null)) {
            return false;
        }
        self.0 == other.0
    }
}

impl Eq for JoinKey {}

impl Hash for JoinKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        for value in &self.0 {
            match value {
                Value::Int(v) => {
                    0u8.hash(state);
                    v.hash(state);
                }
                Value::Float(v) => {
                    1u8.hash(state);
                    v.to_bits().hash(state);
                }
                Value::Bool(v) => {
                    2u8.hash(state);
                    v.hash(state);
                }
                Value::Str(v) => {
                    3u8.hash(state);
                    v.hash(state);
                }
                Value::Null => 4u8.hash(state),
            }
        }
    }
}

/// A register machine executing one batch at a time.
#[derive(Default)]
pub struct Vm {
    registers: HashMap<usize, Vec<Value>>,
    output: Vec<Vec<Value>>,
    join_tables: HashMap<usize, JoinHashTable<JoinKey, Vec<Value>>>,
    /// Instructions executed so far, checked against [`MAX_STEPS`] by
    /// [`Vm::execute`]/[`Vm::run`].
    steps: usize,
}

impl fmt::Debug for Vm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vm")
            .field("registers", &self.registers)
            .field("output", &self.output)
            .field("join_tables", &self.join_tables.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Vm {
    pub fn new() -> Self {
        Vm::default()
    }

    pub fn register(&self, reg: usize) -> Result<&[Value]> {
        self.reg(reg, "register")
    }

    /// Like [`Self::register`], but tags an unknown-register error with the
    /// opcode that requested it, for callers inside [`Self::step`].
    fn reg(&self, reg: usize, opcode: &'static str) -> Result<&[Value]> {
        self.registers
            .get(&reg)
            .map(Vec::as_slice)
            .ok_or(VmError::UnknownRegister {
                opcode,
                register: reg,
            })
    }

    /// Count one more executed instruction, failing once [`MAX_STEPS`] is
    /// exceeded so a pathological or buggy compiled program can't run
    /// forever.
    fn check_step_limit(&mut self, opcode: &'static str) -> Result<()> {
        self.steps += 1;
        if self.steps > MAX_STEPS {
            return Err(VmError::StepLimitExceeded {
                opcode,
                limit: MAX_STEPS,
            });
        }
        Ok(())
    }

    pub fn execute(&mut self, batch: &Batch, program: &[Opcode]) -> Result<()> {
        for op in program {
            self.check_step_limit(op.name())?;
            self.step(batch, op)?;
        }
        Ok(())
    }

    /// Drop every live register (but keep built join tables and collected
    /// output) -- callers switch to this between running a build-side
    /// program and a probe-side program against a different batch/row
    /// count, since [`Opcode::Filter`] and [`Opcode::HashProbe`] reshape
    /// *every* live register and would otherwise choke on leftover
    /// build-side registers with the wrong length.
    pub fn clear_registers(&mut self) {
        self.registers.clear();
    }

    /// Take (and clear) the rows collected so far by [`Opcode::Emit`] --
    /// for callers driving [`Self::execute`] batch-by-batch themselves
    /// (e.g. a bounded scan that stops once enough rows are collected,
    /// #108) rather than via [`Self::run`]/[`run_parallel`].
    pub fn take_output(&mut self) -> Vec<Vec<Value>> {
        std::mem::take(&mut self.output)
    }

    /// Drive `program` across every batch `source` yields, honoring
    /// [`Opcode::Scan`]/[`Opcode::NextSegment`]/[`Opcode::Halt`] control
    /// flow, and return the rows collected by [`Opcode::Emit`].
    pub fn run(&mut self, source: &mut dyn Source, program: &[Opcode]) -> Result<Vec<Vec<Value>>> {
        self.output.clear();
        let mut batch = match source.next_batch() {
            Some(b) => b,
            None => return Ok(Vec::new()),
        };
        let mut pc = 0usize;
        while pc < program.len() {
            self.check_step_limit(program[pc].name())?;
            match &program[pc] {
                Opcode::NextSegment { loop_start } => match source.next_batch() {
                    Some(next) => {
                        batch = next;
                        pc = *loop_start;
                    }
                    None => pc += 1,
                },
                Opcode::Halt => break,
                other => {
                    self.step(&batch, other)?;
                    pc += 1;
                }
            }
        }
        Ok(std::mem::take(&mut self.output))
    }

    fn step(&mut self, batch: &Batch, op: &Opcode) -> Result<()> {
        let opcode = op.name();
        match op {
            Opcode::LoadColumn { reg, column } => {
                let values =
                    batch
                        .columns
                        .get(column.as_ref())
                        .ok_or_else(|| VmError::UnknownColumn {
                            opcode,
                            column: column.to_string(),
                        })?;
                self.registers.insert(*reg, values.clone());
            }
            Opcode::LoadConst { reg, value } => {
                self.registers
                    .insert(*reg, vec![value.clone(); batch.num_rows]);
            }
            Opcode::Map { dst, op, a, b } => {
                let (a_vals, b_vals) = (self.reg(*a, opcode)?, self.reg(*b, opcode)?);
                if a_vals.len() != b_vals.len() {
                    return Err(VmError::RegisterLengthMismatch { opcode });
                }
                let result = a_vals
                    .iter()
                    .zip(b_vals)
                    .map(|(x, y)| apply_map_op(*op, x, y))
                    .collect();
                self.registers.insert(*dst, result);
            }
            Opcode::Filter { predicate } => {
                let mask: Vec<bool> = self
                    .reg(*predicate, opcode)?
                    .iter()
                    .map(|v| matches!(v, Value::Bool(true)))
                    .collect();
                // #110: size each kept-values buffer to the actual survivor
                // count (not the pre-filter length) -- at 50% selectivity on
                // a 10M-row scan, over-allocating by 2x per live register is
                // real peak-RSS waste for no benefit (the excess capacity is
                // never used, just reserved).
                let kept_len = mask.iter().filter(|&&keep| keep).count();
                for values in self.registers.values_mut() {
                    if values.len() != mask.len() {
                        return Err(VmError::RegisterLengthMismatch { opcode });
                    }
                    let mut kept = Vec::with_capacity(kept_len);
                    for (value, keep) in values.drain(..).zip(&mask) {
                        if *keep {
                            kept.push(value);
                        }
                    }
                    *values = kept;
                }
            }
            Opcode::Reduce { func, src, dst } => {
                let result = match src {
                    Some(reg) => reduce_values(*func, self.reg(*reg, opcode)?),
                    None => reduce_count_star(*func, batch.num_rows),
                };
                self.registers.insert(*dst, vec![result]);
            }
            Opcode::GroupReduce {
                group_by,
                aggs,
                agg_dst,
            } => {
                let key_columns: Vec<Vec<Value>> = group_by
                    .iter()
                    .map(|reg| self.reg(*reg, opcode).map(<[Value]>::to_vec))
                    .collect::<Result<_>>()?;
                let num_rows = match key_columns.first() {
                    Some(c) => c.len(),
                    None => match aggs.iter().find_map(|(_, src)| {
                        src.map(|reg| self.reg(reg, opcode).map(<[Value]>::len))
                    }) {
                        Some(len) => len?,
                        None => self
                            .registers
                            .values()
                            .next()
                            .map_or(batch.num_rows, Vec::len),
                    },
                };

                let mut group_index: HashMap<String, usize> = HashMap::new();
                let mut group_keys: Vec<Vec<Value>> = Vec::new();
                let mut row_group: Vec<usize> = Vec::with_capacity(num_rows);
                for row in 0..num_rows {
                    let key: Vec<Value> = key_columns.iter().map(|c| c[row].clone()).collect();
                    let key_str = key
                        .iter()
                        .map(Value::to_string)
                        .collect::<Vec<_>>()
                        .join("\u{0}");
                    let group = *group_index.entry(key_str).or_insert_with(|| {
                        group_keys.push(key);
                        group_keys.len() - 1
                    });
                    row_group.push(group);
                }
                let num_groups = group_keys.len();

                for (i, reg) in group_by.iter().enumerate() {
                    self.registers
                        .insert(*reg, group_keys.iter().map(|k| k[i].clone()).collect());
                }

                for ((func, src), dst) in aggs.iter().zip(agg_dst.iter()) {
                    let mut per_group: Vec<Vec<Value>> = vec![Vec::new(); num_groups];
                    match src {
                        Some(reg) => {
                            // #110: borrow instead of `.to_vec()` -- same
                            // redundant-clone pattern as the old `Emit`.
                            let values = self.reg(*reg, opcode)?;
                            for (row, group) in row_group.iter().enumerate() {
                                per_group[*group].push(values[row].clone());
                            }
                        }
                        None => {
                            for group in &row_group {
                                per_group[*group].push(Value::Null);
                            }
                        }
                    }
                    let result = per_group
                        .iter()
                        .map(|vals| {
                            if src.is_none() {
                                Value::Int(vals.len() as i64)
                            } else {
                                reduce_values(*func, vals)
                            }
                        })
                        .collect();
                    self.registers.insert(*dst, result);
                }
            }
            Opcode::HashBuild {
                key_cols,
                payload_cols,
                table,
            } => {
                let key_columns: Vec<&[Value]> = key_cols
                    .iter()
                    .map(|r| self.reg(*r, opcode))
                    .collect::<Result<_>>()?;
                let payload_columns: Vec<&[Value]> = payload_cols
                    .iter()
                    .map(|r| self.reg(*r, opcode))
                    .collect::<Result<_>>()?;
                let num_rows = key_columns.first().map_or(0, |c| c.len());

                let mut ht: JoinHashTable<JoinKey, Vec<Value>> =
                    JoinHashTable::with_capacity(num_rows);
                for row in 0..num_rows {
                    let key = JoinKey(key_columns.iter().map(|c| c[row].clone()).collect());
                    let payload = payload_columns.iter().map(|c| c[row].clone()).collect();
                    ht.insert(key, payload);
                }
                self.join_tables.insert(*table, ht);
            }
            Opcode::HashProbe {
                key_cols,
                table,
                payload_dst,
                kind,
            } => {
                let key_columns: Vec<&[Value]> = key_cols
                    .iter()
                    .map(|r| self.reg(*r, opcode))
                    .collect::<Result<_>>()?;
                let num_rows = key_columns.first().map_or(0, |c| c.len());
                let ht = self
                    .join_tables
                    .get(table)
                    .ok_or(VmError::UnknownJoinTable {
                        opcode,
                        table: *table,
                    })?;

                // Owned so the borrow of `self.join_tables` (via `ht`) and
                // of `self.registers` (via `key_columns`) both end here,
                // before the reshape below needs to mutably borrow
                // `self.registers`.
                let mut emitted: Vec<(usize, Option<Vec<Value>>)> = Vec::with_capacity(num_rows);
                for row in 0..num_rows {
                    let key = JoinKey(key_columns.iter().map(|c| c[row].clone()).collect());
                    let matches: Vec<&Vec<Value>> = ht.get_all(&key).collect();
                    if matches.is_empty() {
                        if should_emit(*kind, false, false) {
                            emitted.push((row, None));
                        }
                    } else if matches!(kind, JoinKind::Semi) {
                        // At most one row per probe-side match, regardless
                        // of build-side fanout; no payload (see doc comment
                        // on `Opcode::HashProbe`).
                        emitted.push((row, None));
                    } else {
                        for m in matches {
                            if should_emit(*kind, true, true) {
                                emitted.push((row, Some(m.clone())));
                            }
                        }
                    }
                }

                for values in self.registers.values_mut() {
                    if values.len() != num_rows {
                        return Err(VmError::RegisterLengthMismatch { opcode });
                    }
                    *values = emitted
                        .iter()
                        .map(|(row, _)| values[*row].clone())
                        .collect();
                }

                for (i, dst) in payload_dst.iter().enumerate() {
                    let col: Vec<Value> = emitted
                        .iter()
                        .map(|(_, payload)| match payload {
                            Some(p) => p.get(i).cloned().unwrap_or(Value::Null),
                            None => Value::Null,
                        })
                        .collect();
                    self.registers.insert(*dst, col);
                }
            }
            Opcode::Window {
                func,
                arg,
                offset,
                partition_by,
                order_by,
                dst,
            } => {
                let partition_cols: Vec<&[Value]> = partition_by
                    .iter()
                    .map(|r| self.reg(*r, opcode))
                    .collect::<Result<_>>()?;
                let order_cols: Vec<(&[Value], bool)> = order_by
                    .iter()
                    .map(|(r, desc)| self.reg(*r, opcode).map(|c| (c, *desc)))
                    .collect::<Result<_>>()?;
                let arg_col: Option<&[Value]> = match arg {
                    Some(r) => Some(self.reg(*r, opcode)?),
                    None => None,
                };

                let num_rows = partition_cols.first().map_or_else(
                    || {
                        order_cols.first().map_or_else(
                            || arg_col.map_or(batch.num_rows, <[Value]>::len),
                            |(c, _)| c.len(),
                        )
                    },
                    |c| c.len(),
                );

                let result = compute_window(
                    *func,
                    *offset,
                    &partition_cols,
                    &order_cols,
                    arg_col,
                    num_rows,
                );
                self.registers.insert(*dst, result);
            }
            Opcode::Emit { registers } => {
                // #110: borrow each register instead of `.to_vec()`-ing it
                // first -- the transpose loop below already clones every
                // cell once to build each output row, so cloning the whole
                // column again first (the previous `.to_vec()`) doubled the
                // clone cost of every surviving value for no reason.
                let cols: Vec<&[Value]> = registers
                    .iter()
                    .map(|r| self.reg(*r, opcode))
                    .collect::<Result<_>>()?;
                let num_rows = cols.first().map_or(0, |c| c.len());
                let mut rows = Vec::with_capacity(num_rows);
                for row in 0..num_rows {
                    rows.push(cols.iter().map(|c| c[row].clone()).collect());
                }
                drop(cols);
                self.output.extend(rows);
            }
            // Meaningful only as loop markers interpreted by `run` --
            // and `Finalize` is a cross-segment barrier applied once by
            // `crate::vm::engine::run`, never inside a single segment.
            Opcode::Scan | Opcode::NextSegment { .. } | Opcode::Halt | Opcode::Finalize { .. } => {}
        }
        Ok(())
    }
}

/// Ported 1:1 from column-rs's private `compute_window` (its only prior
/// implementation) -- partitions `0..num_rows` by `partition_cols`' tuple
/// (stringified, same non-NULL-safe convention as [`Opcode::GroupReduce`]'s
/// grouping), sorts each partition by `order_cols` via [`compare_for_order`],
/// then computes `func` per row within its partition.
fn compute_window(
    func: WindowFunc,
    offset: Option<i64>,
    partition_cols: &[&[Value]],
    order_cols: &[(&[Value], bool)],
    arg_col: Option<&[Value]>,
    num_rows: usize,
) -> Vec<Value> {
    let mut partitions: HashMap<String, Vec<usize>> = HashMap::new();
    let mut partition_order: Vec<String> = Vec::new();
    for row in 0..num_rows {
        let key = partition_cols
            .iter()
            .map(|c| c[row].to_string())
            .collect::<Vec<_>>()
            .join("\u{0}");
        if !partitions.contains_key(&key) {
            partition_order.push(key.clone());
        }
        partitions.entry(key).or_default().push(row);
    }

    let mut output = vec![Value::Null; num_rows];
    for key in &partition_order {
        let mut indices = partitions[key].clone();
        indices.sort_by(|&a, &b| {
            for (col, descending) in order_cols {
                let ord = compare_for_order(&col[a], &col[b], *descending);
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            std::cmp::Ordering::Equal
        });

        match func {
            WindowFunc::RowNumber => {
                for (pos, &row) in indices.iter().enumerate() {
                    output[row] = Value::Int((pos + 1) as i64);
                }
            }
            WindowFunc::Rank | WindowFunc::DenseRank => {
                let mut rank = 0i64;
                let mut dense = 0i64;
                let mut prev: Option<usize> = None;
                for (pos, &row) in indices.iter().enumerate() {
                    let is_new = match prev {
                        None => true,
                        Some(prev_row) => order_cols
                            .iter()
                            .any(|(col, _)| col[row].to_string() != col[prev_row].to_string()),
                    };
                    if is_new {
                        rank = (pos + 1) as i64;
                        dense += 1;
                    }
                    output[row] = Value::Int(if func == WindowFunc::Rank {
                        rank
                    } else {
                        dense
                    });
                    prev = Some(row);
                }
            }
            WindowFunc::Lag | WindowFunc::Lead => {
                let offset = offset.unwrap_or(1);
                let arg = arg_col.expect("Lag/Lead always have an argument column");
                let n = indices.len() as i64;
                for (pos, &row) in indices.iter().enumerate() {
                    let target = if func == WindowFunc::Lag {
                        pos as i64 - offset
                    } else {
                        pos as i64 + offset
                    };
                    output[row] = if target >= 0 && target < n {
                        arg[indices[target as usize]].clone()
                    } else {
                        Value::Null
                    };
                }
            }
            WindowFunc::FirstValue => {
                let arg = arg_col.expect("FirstValue always has an argument column");
                if let Some(&first) = indices.first() {
                    let v = arg[first].clone();
                    for &row in &indices {
                        output[row] = v.clone();
                    }
                }
            }
            WindowFunc::LastValue => {
                let arg = arg_col.expect("LastValue always has an argument column");
                for &row in &indices {
                    output[row] = arg[row].clone();
                }
            }
            WindowFunc::Sum | WindowFunc::Avg | WindowFunc::Count => {
                if order_cols.is_empty() {
                    let agg = whole_partition_aggregate(func, arg_col, &indices);
                    for &row in &indices {
                        output[row] = agg.clone();
                    }
                } else {
                    let mut running_sum = 0.0;
                    let mut running_count = 0i64;
                    for &row in &indices {
                        let counted = match arg_col {
                            Some(a) => !matches!(a[row], Value::Null),
                            None => true,
                        };
                        if counted {
                            running_count += 1;
                            if let Some(a) = arg_col {
                                if let Some(v) = a[row].as_f64() {
                                    running_sum += v;
                                }
                            }
                        }
                        output[row] = match func {
                            WindowFunc::Count => Value::Int(running_count),
                            WindowFunc::Sum => {
                                if running_count > 0 {
                                    Value::Float(running_sum)
                                } else {
                                    Value::Null
                                }
                            }
                            WindowFunc::Avg => {
                                if running_count > 0 {
                                    Value::Float(running_sum / running_count as f64)
                                } else {
                                    Value::Null
                                }
                            }
                            _ => unreachable!(),
                        };
                    }
                }
            }
        }
    }
    output
}

/// `SUM`/`AVG`/`COUNT OVER (PARTITION BY ...)` with no `ORDER BY`: the
/// default frame is the whole partition, so every row in it gets the same
/// aggregate value.
fn whole_partition_aggregate(
    func: WindowFunc,
    arg_col: Option<&[Value]>,
    indices: &[usize],
) -> Value {
    if func == WindowFunc::Count {
        let count = match arg_col {
            Some(a) => indices
                .iter()
                .filter(|&&row| !matches!(a[row], Value::Null))
                .count(),
            None => indices.len(),
        };
        return Value::Int(count as i64);
    }
    let values: Vec<f64> = indices
        .iter()
        .filter_map(|&row| arg_col.and_then(|a| a[row].as_f64()))
        .collect();
    if values.is_empty() {
        return Value::Null;
    }
    match func {
        WindowFunc::Sum => Value::Float(values.iter().sum()),
        WindowFunc::Avg => Value::Float(values.iter().sum::<f64>() / values.len() as f64),
        _ => unreachable!(),
    }
}

fn reduce_count_star(func: AggFunc, num_rows: usize) -> Value {
    match func {
        AggFunc::Count => Value::Int(num_rows as i64),
        _ => Value::Null,
    }
}

fn reduce_values(func: AggFunc, values: &[Value]) -> Value {
    let non_null: Vec<f64> = values.iter().filter_map(Value::as_f64).collect();
    match func {
        AggFunc::Count => {
            Value::Int(values.iter().filter(|v| !matches!(v, Value::Null)).count() as i64)
        }
        AggFunc::Sum => {
            if non_null.is_empty() {
                Value::Null
            } else {
                Value::Float(non_null.iter().sum())
            }
        }
        AggFunc::Avg => {
            if non_null.is_empty() {
                Value::Null
            } else {
                Value::Float(non_null.iter().sum::<f64>() / non_null.len() as f64)
            }
        }
        AggFunc::Min => non_null
            .into_iter()
            .fold(None, |acc: Option<f64>, v| {
                Some(acc.map_or(v, |a| a.min(v)))
            })
            .map_or(Value::Null, Value::Float),
        AggFunc::Max => non_null
            .into_iter()
            .fold(None, |acc: Option<f64>, v| {
                Some(acc.map_or(v, |a| a.max(v)))
            })
            .map_or(Value::Null, Value::Float),
    }
}

fn apply_map_op(op: MapOp, a: &Value, b: &Value) -> Value {
    if matches!(op, MapOp::IsNull) {
        return Value::Bool(matches!(a, Value::Null));
    }
    if matches!(op, MapOp::IsNotNull) {
        return Value::Bool(!matches!(a, Value::Null));
    }
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Value::Null;
    }
    match op {
        MapOp::Add | MapOp::Sub | MapOp::Mul | MapOp::Div => {
            let (x, y) = match (a.as_f64(), b.as_f64()) {
                (Some(x), Some(y)) => (x, y),
                _ => return Value::Null,
            };
            let result = match op {
                MapOp::Add => x + y,
                MapOp::Sub => x - y,
                MapOp::Mul => x * y,
                MapOp::Div => x / y,
                _ => unreachable!(),
            };
            if matches!(a, Value::Int(_)) && matches!(b, Value::Int(_)) && op != MapOp::Div {
                Value::Int(result as i64)
            } else {
                Value::Float(result)
            }
        }
        MapOp::Eq | MapOp::Ne | MapOp::Lt | MapOp::Le | MapOp::Gt | MapOp::Ge => {
            let ordering = compare_values(a, b);
            let result = match op {
                MapOp::Eq => ordering == Some(std::cmp::Ordering::Equal),
                MapOp::Ne => ordering != Some(std::cmp::Ordering::Equal),
                MapOp::Lt => ordering == Some(std::cmp::Ordering::Less),
                MapOp::Le => matches!(
                    ordering,
                    Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
                ),
                MapOp::Gt => ordering == Some(std::cmp::Ordering::Greater),
                MapOp::Ge => matches!(
                    ordering,
                    Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
                ),
                _ => unreachable!(),
            };
            Value::Bool(result)
        }
        MapOp::And => Value::Bool(as_bool(a) && as_bool(b)),
        MapOp::Or => Value::Bool(as_bool(a) || as_bool(b)),
        MapOp::Not => Value::Bool(!as_bool(a)),
        MapOp::Concat => Value::Str(Cow::Owned(format!("{a}{b}"))),
        MapOp::Neg => match a {
            Value::Int(v) => Value::Int(-v),
            Value::Float(v) => Value::Float(-v),
            _ => Value::Null,
        },
        MapOp::IsNull | MapOp::IsNotNull => unreachable!("handled above"),
    }
}

fn as_bool(v: &Value) -> bool {
    matches!(v, Value::Bool(true))
}

fn compare_values(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Str(x), Value::Str(y)) => Some(x.cmp(y)),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        _ => a.as_f64()?.partial_cmp(&b.as_f64()?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_column_copies_batch_values_into_register() {
        let batch =
            Batch::new(3).with_column("id", vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[Opcode::LoadColumn {
                reg: 0,
                column: "id".into(),
            }],
        )
        .unwrap();
        assert_eq!(
            vm.register(0).unwrap(),
            &[Value::Int(1), Value::Int(2), Value::Int(3)]
        );
    }

    #[test]
    fn load_const_broadcasts_to_batch_length() {
        let batch = Batch::new(3);
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[Opcode::LoadConst {
                reg: 0,
                value: Value::Int(10),
            }],
        )
        .unwrap();
        assert_eq!(
            vm.register(0).unwrap(),
            &[Value::Int(10), Value::Int(10), Value::Int(10)]
        );
    }

    #[test]
    fn load_column_errors_on_unknown_column() {
        let batch = Batch::new(1);
        let mut vm = Vm::new();
        let err = vm
            .execute(
                &batch,
                &[Opcode::LoadColumn {
                    reg: 0,
                    column: "missing".into(),
                }],
            )
            .unwrap_err();
        assert_eq!(
            err,
            VmError::UnknownColumn {
                opcode: "LoadColumn",
                column: "missing".into()
            }
        );
    }

    #[test]
    fn map_add_promotes_to_float_on_division() {
        let batch = Batch::new(2);
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[
                Opcode::LoadConst {
                    reg: 0,
                    value: Value::Int(10),
                },
                Opcode::LoadConst {
                    reg: 1,
                    value: Value::Int(4),
                },
                Opcode::Map {
                    dst: 2,
                    op: MapOp::Add,
                    a: 0,
                    b: 1,
                },
                Opcode::Map {
                    dst: 3,
                    op: MapOp::Div,
                    a: 0,
                    b: 1,
                },
            ],
        )
        .unwrap();
        assert_eq!(vm.register(2).unwrap(), &[Value::Int(14), Value::Int(14)]);
        assert_eq!(
            vm.register(3).unwrap(),
            &[Value::Float(2.5), Value::Float(2.5)]
        );
    }

    #[test]
    fn map_comparison_produces_bool() {
        let batch = Batch::new(1).with_column("amount", vec![Value::Int(15)]);
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[
                Opcode::LoadColumn {
                    reg: 0,
                    column: "amount".into(),
                },
                Opcode::LoadConst {
                    reg: 1,
                    value: Value::Int(10),
                },
                Opcode::Map {
                    dst: 2,
                    op: MapOp::Gt,
                    a: 0,
                    b: 1,
                },
            ],
        )
        .unwrap();
        assert_eq!(vm.register(2).unwrap(), &[Value::Bool(true)]);
    }

    #[test]
    fn map_null_propagates() {
        let batch = Batch::new(1);
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[
                Opcode::LoadConst {
                    reg: 0,
                    value: Value::Null,
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
            ],
        )
        .unwrap();
        assert_eq!(vm.register(2).unwrap(), &[Value::Null]);
    }

    #[test]
    fn map_concat_stringifies_both_operands() {
        let batch = Batch::new(1);
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[
                Opcode::LoadConst {
                    reg: 0,
                    value: Value::Int(1),
                },
                Opcode::LoadConst {
                    reg: 1,
                    value: Value::Str("x".into()),
                },
                Opcode::Map {
                    dst: 2,
                    op: MapOp::Concat,
                    a: 0,
                    b: 1,
                },
            ],
        )
        .unwrap();
        assert_eq!(vm.register(2).unwrap(), &[Value::Str("1x".into())]);
    }

    #[test]
    fn map_concat_null_propagates() {
        let batch = Batch::new(1);
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[
                Opcode::LoadConst {
                    reg: 0,
                    value: Value::Str("a".into()),
                },
                Opcode::LoadConst {
                    reg: 1,
                    value: Value::Null,
                },
                Opcode::Map {
                    dst: 2,
                    op: MapOp::Concat,
                    a: 0,
                    b: 1,
                },
            ],
        )
        .unwrap();
        assert_eq!(vm.register(2).unwrap(), &[Value::Null]);
    }

    #[test]
    fn map_neg_negates_int_and_float() {
        let batch = Batch::new(1);
        let mut vm = Vm::new();
        for (input, expected) in [
            (Value::Int(5), Value::Int(-5)),
            (Value::Float(2.5), Value::Float(-2.5)),
        ] {
            vm.execute(
                &batch,
                &[
                    Opcode::LoadConst {
                        reg: 0,
                        value: input,
                    },
                    Opcode::Map {
                        dst: 1,
                        op: MapOp::Neg,
                        a: 0,
                        b: 0,
                    },
                ],
            )
            .unwrap();
            assert_eq!(vm.register(1).unwrap(), &[expected]);
        }
    }

    #[test]
    fn map_neg_non_numeric_is_null() {
        let batch = Batch::new(1);
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[
                Opcode::LoadConst {
                    reg: 0,
                    value: Value::Str("x".into()),
                },
                Opcode::Map {
                    dst: 1,
                    op: MapOp::Neg,
                    a: 0,
                    b: 0,
                },
            ],
        )
        .unwrap();
        assert_eq!(vm.register(1).unwrap(), &[Value::Null]);
    }

    #[test]
    fn map_length_mismatch_errors() {
        let batch = Batch::new(2).with_column("a", vec![Value::Int(1), Value::Int(2)]);
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[Opcode::LoadColumn {
                reg: 0,
                column: "a".into(),
            }],
        )
        .unwrap();
        vm.registers.insert(1, vec![Value::Int(1)]);
        let err = vm
            .step(
                &batch,
                &Opcode::Map {
                    dst: 2,
                    op: MapOp::Add,
                    a: 0,
                    b: 1,
                },
            )
            .unwrap_err();
        assert_eq!(err, VmError::RegisterLengthMismatch { opcode: "Map" });
    }

    #[test]
    fn filter_keeps_only_true_rows_across_all_registers() {
        let batch = Batch::new(3)
            .with_column("id", vec![Value::Int(1), Value::Int(2), Value::Int(3)])
            .with_column(
                "amount",
                vec![Value::Int(5), Value::Int(15), Value::Int(25)],
            );
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[
                Opcode::LoadColumn {
                    reg: 0,
                    column: "id".into(),
                },
                Opcode::LoadColumn {
                    reg: 1,
                    column: "amount".into(),
                },
                Opcode::LoadConst {
                    reg: 2,
                    value: Value::Int(10),
                },
                Opcode::Map {
                    dst: 3,
                    op: MapOp::Gt,
                    a: 1,
                    b: 2,
                },
                Opcode::Filter { predicate: 3 },
            ],
        )
        .unwrap();
        assert_eq!(vm.register(0).unwrap(), &[Value::Int(2), Value::Int(3)]);
        assert_eq!(vm.register(1).unwrap(), &[Value::Int(15), Value::Int(25)]);
    }

    #[test]
    fn reduce_sum_avg_min_max_skip_nulls() {
        let batch = Batch::new(4).with_column(
            "amount",
            vec![Value::Int(10), Value::Null, Value::Int(20), Value::Int(30)],
        );
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[Opcode::LoadColumn {
                reg: 0,
                column: "amount".into(),
            }],
        )
        .unwrap();

        for (func, expected) in [
            (AggFunc::Sum, Value::Float(60.0)),
            (AggFunc::Avg, Value::Float(20.0)),
            (AggFunc::Min, Value::Float(10.0)),
            (AggFunc::Max, Value::Float(30.0)),
            (AggFunc::Count, Value::Int(3)),
        ] {
            vm.step(
                &batch,
                &Opcode::Reduce {
                    func,
                    src: Some(0),
                    dst: 1,
                },
            )
            .unwrap();
            assert_eq!(vm.register(1).unwrap(), &[expected], "{func:?}");
        }
    }

    #[test]
    fn reduce_count_star_counts_rows_not_values() {
        let batch = Batch::new(5);
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[Opcode::Reduce {
                func: AggFunc::Count,
                src: None,
                dst: 0,
            }],
        )
        .unwrap();
        assert_eq!(vm.register(0).unwrap(), &[Value::Int(5)]);
    }

    #[test]
    fn reduce_sum_of_all_nulls_is_null() {
        let batch = Batch::new(2).with_column("amount", vec![Value::Null, Value::Null]);
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[
                Opcode::LoadColumn {
                    reg: 0,
                    column: "amount".into(),
                },
                Opcode::Reduce {
                    func: AggFunc::Sum,
                    src: Some(0),
                    dst: 1,
                },
            ],
        )
        .unwrap();
        assert_eq!(vm.register(1).unwrap(), &[Value::Null]);
    }

    #[test]
    fn group_reduce_hash_aggregates_by_key() {
        let batch = Batch::new(4)
            .with_column(
                "region",
                vec![
                    Value::Str("east".into()),
                    Value::Str("west".into()),
                    Value::Str("east".into()),
                    Value::Str("west".into()),
                ],
            )
            .with_column(
                "amount",
                vec![
                    Value::Int(10),
                    Value::Int(5),
                    Value::Int(20),
                    Value::Int(15),
                ],
            );
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[
                Opcode::LoadColumn {
                    reg: 0,
                    column: "region".into(),
                },
                Opcode::LoadColumn {
                    reg: 1,
                    column: "amount".into(),
                },
                Opcode::GroupReduce {
                    group_by: vec![0].into(),
                    aggs: vec![(AggFunc::Sum, Some(1)), (AggFunc::Count, None)].into(),
                    agg_dst: vec![2, 3].into(),
                },
            ],
        )
        .unwrap();
        assert_eq!(
            vm.register(0).unwrap(),
            &[Value::Str("east".into()), Value::Str("west".into())]
        );
        assert_eq!(
            vm.register(2).unwrap(),
            &[Value::Float(30.0), Value::Float(20.0)]
        );
        assert_eq!(vm.register(3).unwrap(), &[Value::Int(2), Value::Int(2)]);
    }

    fn build_and_probe(kind: JoinKind, left: Batch, right: Batch) -> Vm {
        let mut vm = Vm::new();
        vm.execute(
            &right,
            &[
                Opcode::LoadColumn {
                    reg: 10,
                    column: "rkey".into(),
                },
                Opcode::LoadColumn {
                    reg: 11,
                    column: "rval".into(),
                },
                Opcode::HashBuild {
                    key_cols: vec![10].into(),
                    payload_cols: vec![11].into(),
                    table: 0,
                },
            ],
        )
        .unwrap();
        vm.clear_registers();
        vm.execute(
            &left,
            &[
                Opcode::LoadColumn {
                    reg: 0,
                    column: "lkey".into(),
                },
                Opcode::HashProbe {
                    key_cols: vec![0].into(),
                    table: 0,
                    payload_dst: vec![1].into(),
                    kind,
                },
            ],
        )
        .unwrap();
        vm
    }

    #[test]
    fn hash_probe_inner_join_keeps_only_matches() {
        let left =
            Batch::new(3).with_column("lkey", vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        let right = Batch::new(2)
            .with_column("rkey", vec![Value::Int(2), Value::Int(3)])
            .with_column("rval", vec![Value::Str("b".into()), Value::Str("c".into())]);
        let vm = build_and_probe(JoinKind::Inner, left, right);
        assert_eq!(vm.register(0).unwrap(), &[Value::Int(2), Value::Int(3)]);
        assert_eq!(
            vm.register(1).unwrap(),
            &[Value::Str("b".into()), Value::Str("c".into())]
        );
    }

    #[test]
    fn hash_probe_inner_join_fans_out_duplicate_build_keys() {
        let left = Batch::new(1).with_column("lkey", vec![Value::Int(1)]);
        let right = Batch::new(2)
            .with_column("rkey", vec![Value::Int(1), Value::Int(1)])
            .with_column("rval", vec![Value::Str("a".into()), Value::Str("b".into())]);
        let vm = build_and_probe(JoinKind::Inner, left, right);
        assert_eq!(vm.register(0).unwrap(), &[Value::Int(1), Value::Int(1)]);
        assert_eq!(
            vm.register(1).unwrap(),
            &[Value::Str("a".into()), Value::Str("b".into())]
        );
    }

    #[test]
    fn hash_probe_left_join_null_fills_unmatched_rows() {
        let left =
            Batch::new(3).with_column("lkey", vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        let right = Batch::new(1)
            .with_column("rkey", vec![Value::Int(2)])
            .with_column("rval", vec![Value::Str("b".into())]);
        let vm = build_and_probe(JoinKind::Left, left, right);
        assert_eq!(
            vm.register(0).unwrap(),
            &[Value::Int(1), Value::Int(2), Value::Int(3)]
        );
        assert_eq!(
            vm.register(1).unwrap(),
            &[Value::Null, Value::Str("b".into()), Value::Null]
        );
    }

    #[test]
    fn hash_probe_null_keys_never_match() {
        let left = Batch::new(1).with_column("lkey", vec![Value::Null]);
        let right = Batch::new(1)
            .with_column("rkey", vec![Value::Null])
            .with_column("rval", vec![Value::Str("x".into())]);
        let vm = build_and_probe(JoinKind::Inner, left, right);
        assert!(vm.register(0).unwrap().is_empty());
    }

    #[test]
    fn hash_probe_semi_join_emits_left_row_once_per_match_group() {
        let left = Batch::new(2).with_column("lkey", vec![Value::Int(1), Value::Int(2)]);
        let right = Batch::new(2)
            .with_column("rkey", vec![Value::Int(1), Value::Int(1)])
            .with_column("rval", vec![Value::Str("a".into()), Value::Str("b".into())]);
        let vm = build_and_probe(JoinKind::Semi, left, right);
        assert_eq!(vm.register(0).unwrap(), &[Value::Int(1)]);
        assert_eq!(vm.register(1).unwrap(), &[Value::Null]);
    }

    #[test]
    fn hash_probe_anti_join_keeps_only_unmatched_rows() {
        let left =
            Batch::new(3).with_column("lkey", vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        let right = Batch::new(1)
            .with_column("rkey", vec![Value::Int(2)])
            .with_column("rval", vec![Value::Str("b".into())]);
        let vm = build_and_probe(JoinKind::Anti, left, right);
        assert_eq!(vm.register(0).unwrap(), &[Value::Int(1), Value::Int(3)]);
    }

    #[test]
    fn hash_probe_errors_on_unknown_join_table() {
        let batch = Batch::new(1).with_column("lkey", vec![Value::Int(1)]);
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[Opcode::LoadColumn {
                reg: 0,
                column: "lkey".into(),
            }],
        )
        .unwrap();
        let err = vm
            .step(
                &batch,
                &Opcode::HashProbe {
                    key_cols: vec![0].into(),
                    table: 42,
                    payload_dst: vec![1].into(),
                    kind: JoinKind::Inner,
                },
            )
            .unwrap_err();
        assert_eq!(
            err,
            VmError::UnknownJoinTable {
                opcode: "HashProbe",
                table: 42
            }
        );
    }

    #[test]
    fn unknown_register_error_carries_opcode_context() {
        let batch = Batch::new(1);
        let mut vm = Vm::new();
        let err = vm
            .step(
                &batch,
                &Opcode::Map {
                    dst: 2,
                    op: MapOp::Add,
                    a: 0,
                    b: 1,
                },
            )
            .unwrap_err();
        assert_eq!(
            err,
            VmError::UnknownRegister {
                opcode: "Map",
                register: 0
            }
        );
    }

    #[test]
    fn execute_errors_once_step_limit_exceeded() {
        let batch = Batch::new(1);
        let mut vm = Vm::new();
        vm.steps = MAX_STEPS;
        let err = vm
            .execute(
                &batch,
                &[Opcode::LoadConst {
                    reg: 0,
                    value: Value::Int(1),
                }],
            )
            .unwrap_err();
        assert_eq!(
            err,
            VmError::StepLimitExceeded {
                opcode: "LoadConst",
                limit: MAX_STEPS
            }
        );
    }

    #[test]
    fn run_errors_once_step_limit_exceeded() {
        let batches = vec![Batch::new(1).with_column("id", vec![Value::Int(1)])];
        let mut source = VecSource::new(batches);
        let mut vm = Vm::new();
        vm.steps = MAX_STEPS;
        let program = vec![Opcode::Halt];
        let err = vm.run(&mut source, &program).unwrap_err();
        assert_eq!(
            err,
            VmError::StepLimitExceeded {
                opcode: "Halt",
                limit: MAX_STEPS
            }
        );
    }

    fn run_window(batch: &Batch, op: Opcode) -> Vec<Value> {
        let mut vm = Vm::new();
        for name in batch.columns.keys() {
            vm.step(
                batch,
                &Opcode::LoadColumn {
                    reg: name_to_reg(name),
                    column: name.clone().into(),
                },
            )
            .unwrap();
        }
        vm.step(batch, &op).unwrap();
        let Opcode::Window { dst, .. } = op else {
            panic!("expected Opcode::Window")
        };
        vm.register(dst).unwrap().to_vec()
    }

    // Deterministic per-name register numbers so `run_window`'s test
    // harness doesn't need the caller to hand-assign one per column.
    fn name_to_reg(name: &str) -> usize {
        match name {
            "part" => 0,
            "ord" => 1,
            "val" => 2,
            other => panic!("unexpected test column: {other}"),
        }
    }

    #[test]
    fn window_row_number_restarts_per_partition() {
        let batch = Batch::new(4)
            .with_column(
                "part",
                vec![
                    Value::Str("a".into()),
                    Value::Str("a".into()),
                    Value::Str("b".into()),
                    Value::Str("b".into()),
                ],
            )
            .with_column(
                "ord",
                vec![Value::Int(1), Value::Int(2), Value::Int(1), Value::Int(2)],
            );
        let rows = run_window(
            &batch,
            Opcode::Window {
                func: WindowFunc::RowNumber,
                arg: None,
                offset: None,
                partition_by: vec![0].into(),
                order_by: vec![(1, false)].into(),
                dst: 10,
            },
        );
        assert_eq!(
            rows,
            vec![Value::Int(1), Value::Int(2), Value::Int(1), Value::Int(2)]
        );
    }

    #[test]
    fn window_rank_and_dense_rank_handle_ties() {
        let batch = Batch::new(4).with_column(
            "ord",
            vec![
                Value::Int(10),
                Value::Int(10),
                Value::Int(20),
                Value::Int(30),
            ],
        );
        let rank = run_window(
            &batch,
            Opcode::Window {
                func: WindowFunc::Rank,
                arg: None,
                offset: None,
                partition_by: vec![].into(),
                order_by: vec![(1, false)].into(),
                dst: 10,
            },
        );
        assert_eq!(
            rank,
            vec![Value::Int(1), Value::Int(1), Value::Int(3), Value::Int(4)]
        );

        let dense = run_window(
            &batch,
            Opcode::Window {
                func: WindowFunc::DenseRank,
                arg: None,
                offset: None,
                partition_by: vec![].into(),
                order_by: vec![(1, false)].into(),
                dst: 10,
            },
        );
        assert_eq!(
            dense,
            vec![Value::Int(1), Value::Int(1), Value::Int(2), Value::Int(3)]
        );
    }

    #[test]
    fn window_lag_and_lead_default_offset_one() {
        let batch = Batch::new(3)
            .with_column("ord", vec![Value::Int(1), Value::Int(2), Value::Int(3)])
            .with_column(
                "val",
                vec![
                    Value::Str("x".into()),
                    Value::Str("y".into()),
                    Value::Str("z".into()),
                ],
            );
        let lag = run_window(
            &batch,
            Opcode::Window {
                func: WindowFunc::Lag,
                arg: Some(2),
                offset: None,
                partition_by: vec![].into(),
                order_by: vec![(1, false)].into(),
                dst: 10,
            },
        );
        assert_eq!(
            lag,
            vec![Value::Null, Value::Str("x".into()), Value::Str("y".into())]
        );

        let lead = run_window(
            &batch,
            Opcode::Window {
                func: WindowFunc::Lead,
                arg: Some(2),
                offset: None,
                partition_by: vec![].into(),
                order_by: vec![(1, false)].into(),
                dst: 10,
            },
        );
        assert_eq!(
            lead,
            vec![Value::Str("y".into()), Value::Str("z".into()), Value::Null]
        );
    }

    #[test]
    fn window_first_value_and_last_value() {
        let batch = Batch::new(3)
            .with_column("ord", vec![Value::Int(1), Value::Int(2), Value::Int(3)])
            .with_column(
                "val",
                vec![
                    Value::Str("x".into()),
                    Value::Str("y".into()),
                    Value::Str("z".into()),
                ],
            );
        let first = run_window(
            &batch,
            Opcode::Window {
                func: WindowFunc::FirstValue,
                arg: Some(2),
                offset: None,
                partition_by: vec![].into(),
                order_by: vec![(1, false)].into(),
                dst: 10,
            },
        );
        assert_eq!(
            first,
            vec![
                Value::Str("x".into()),
                Value::Str("x".into()),
                Value::Str("x".into())
            ]
        );

        let last = run_window(
            &batch,
            Opcode::Window {
                func: WindowFunc::LastValue,
                arg: Some(2),
                offset: None,
                partition_by: vec![].into(),
                order_by: vec![(1, false)].into(),
                dst: 10,
            },
        );
        assert_eq!(
            last,
            vec![
                Value::Str("x".into()),
                Value::Str("y".into()),
                Value::Str("z".into())
            ]
        );
    }

    #[test]
    fn window_sum_over_whole_partition_without_order_by_broadcasts() {
        let batch =
            Batch::new(3).with_column("val", vec![Value::Int(10), Value::Int(20), Value::Null]);
        let sums = run_window(
            &batch,
            Opcode::Window {
                func: WindowFunc::Sum,
                arg: Some(2),
                offset: None,
                partition_by: vec![].into(),
                order_by: vec![].into(),
                dst: 10,
            },
        );
        assert_eq!(
            sums,
            vec![Value::Float(30.0), Value::Float(30.0), Value::Float(30.0)]
        );
    }

    #[test]
    fn window_sum_with_order_by_is_a_running_total() {
        let batch = Batch::new(3)
            .with_column("ord", vec![Value::Int(1), Value::Int(2), Value::Int(3)])
            .with_column("val", vec![Value::Int(10), Value::Int(20), Value::Int(30)]);
        let running = run_window(
            &batch,
            Opcode::Window {
                func: WindowFunc::Sum,
                arg: Some(2),
                offset: None,
                partition_by: vec![].into(),
                order_by: vec![(1, false)].into(),
                dst: 10,
            },
        );
        assert_eq!(
            running,
            vec![Value::Float(10.0), Value::Float(30.0), Value::Float(60.0)]
        );
    }

    #[test]
    fn window_count_with_no_arg_counts_rows() {
        let batch =
            Batch::new(3).with_column("ord", vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        let counts = run_window(
            &batch,
            Opcode::Window {
                func: WindowFunc::Count,
                arg: None,
                offset: None,
                partition_by: vec![].into(),
                order_by: vec![(1, false)].into(),
                dst: 10,
            },
        );
        assert_eq!(counts, vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
    }

    #[test]
    fn window_avg_of_all_nulls_is_null() {
        let batch = Batch::new(2).with_column("val", vec![Value::Null, Value::Null]);
        let avgs = run_window(
            &batch,
            Opcode::Window {
                func: WindowFunc::Avg,
                arg: Some(2),
                offset: None,
                partition_by: vec![].into(),
                order_by: vec![].into(),
                dst: 10,
            },
        );
        assert_eq!(avgs, vec![Value::Null, Value::Null]);
    }

    struct VecSource(std::vec::IntoIter<Batch>);

    impl VecSource {
        fn new(batches: Vec<Batch>) -> Self {
            VecSource(batches.into_iter())
        }
    }

    impl Source for VecSource {
        fn next_batch(&mut self) -> Option<Batch> {
            self.0.next()
        }
    }

    #[test]
    fn run_scans_all_segments_and_emits_rows() {
        let batches = vec![
            Batch::new(2).with_column("id", vec![Value::Int(1), Value::Int(2)]),
            Batch::new(1).with_column("id", vec![Value::Int(3)]),
        ];
        let mut source = VecSource::new(batches);
        let mut vm = Vm::new();
        let program = vec![
            Opcode::Scan,
            Opcode::LoadColumn {
                reg: 0,
                column: "id".into(),
            },
            Opcode::Emit {
                registers: vec![0].into(),
            },
            Opcode::NextSegment { loop_start: 1 },
            Opcode::Halt,
        ];
        let rows = vm.run(&mut source, &program).unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1)],
                vec![Value::Int(2)],
                vec![Value::Int(3)]
            ]
        );
    }

    #[test]
    fn run_returns_empty_when_source_has_no_batches() {
        let mut source = VecSource::new(vec![]);
        let mut vm = Vm::new();
        let rows = vm.run(&mut source, &[Opcode::Halt]).unwrap();
        assert!(rows.is_empty());
    }

    struct InMemorySegment(Batch);

    impl Segment for InMemorySegment {
        fn load(&self) -> Batch {
            self.0.clone()
        }
    }

    #[test]
    fn run_parallel_scans_all_segments_in_order() {
        let segments: Vec<Box<dyn Segment>> = (0..8)
            .map(|i| {
                Box::new(InMemorySegment(
                    Batch::new(1).with_column("id", vec![Value::Int(i)]),
                )) as Box<dyn Segment>
            })
            .collect();
        let program = vec![
            Opcode::LoadColumn {
                reg: 0,
                column: "id".into(),
            },
            Opcode::Emit {
                registers: vec![0].into(),
            },
        ];
        let rows = run_parallel(&segments, &program).unwrap();
        let ids: Vec<i64> = rows
            .iter()
            .map(|r| match &r[0] {
                Value::Int(v) => *v,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(ids, (0..8).collect::<Vec<_>>());
    }

    #[test]
    fn run_parallel_applies_filter_per_segment() {
        let segments: Vec<Box<dyn Segment>> = vec![
            Box::new(InMemorySegment(
                Batch::new(2).with_column("amount", vec![Value::Int(5), Value::Int(15)]),
            )),
            Box::new(InMemorySegment(
                Batch::new(2).with_column("amount", vec![Value::Int(25), Value::Int(3)]),
            )),
        ];
        let program = vec![
            Opcode::LoadColumn {
                reg: 0,
                column: "amount".into(),
            },
            Opcode::LoadConst {
                reg: 1,
                value: Value::Int(10),
            },
            Opcode::Map {
                dst: 2,
                op: MapOp::Gt,
                a: 0,
                b: 1,
            },
            Opcode::Filter { predicate: 2 },
            Opcode::Emit {
                registers: vec![0].into(),
            },
        ];
        let rows = run_parallel(&segments, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Int(15)], vec![Value::Int(25)]]);
    }

    #[test]
    fn run_parallel_top_n_picks_largest_across_segments_descending() {
        let segments: Vec<Box<dyn Segment>> = vec![
            Box::new(InMemorySegment(Batch::new(3).with_column(
                "amount",
                vec![Value::Int(5), Value::Int(15), Value::Null],
            ))),
            Box::new(InMemorySegment(Batch::new(3).with_column(
                "amount",
                vec![Value::Int(25), Value::Int(3), Value::Int(20)],
            ))),
        ];
        let program = vec![
            Opcode::LoadColumn {
                reg: 0,
                column: "amount".into(),
            },
            Opcode::Emit {
                registers: vec![0].into(),
            },
        ];
        let spec = TopN {
            col: 0,
            descending: true,
            limit: 3,
        };
        let rows = run_parallel_top_n(&segments, &program, &spec).unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(25)],
                vec![Value::Int(20)],
                vec![Value::Int(15)]
            ]
        );
    }

    #[test]
    fn run_parallel_top_n_sorts_nulls_last_ascending() {
        let segments: Vec<Box<dyn Segment>> =
            vec![Box::new(InMemorySegment(Batch::new(4).with_column(
                "amount",
                vec![Value::Int(5), Value::Null, Value::Int(1), Value::Int(9)],
            )))];
        let program = vec![
            Opcode::LoadColumn {
                reg: 0,
                column: "amount".into(),
            },
            Opcode::Emit {
                registers: vec![0].into(),
            },
        ];
        let spec = TopN {
            col: 0,
            descending: false,
            limit: 3,
        };
        let rows = run_parallel_top_n(&segments, &program, &spec).unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1)],
                vec![Value::Int(5)],
                vec![Value::Int(9)]
            ]
        );
    }

    #[test]
    fn run_parallel_top_n_limit_larger_than_row_count_returns_all_sorted() {
        let segments: Vec<Box<dyn Segment>> = vec![Box::new(InMemorySegment(
            Batch::new(2).with_column("amount", vec![Value::Int(2), Value::Int(1)]),
        ))];
        let program = vec![
            Opcode::LoadColumn {
                reg: 0,
                column: "amount".into(),
            },
            Opcode::Emit {
                registers: vec![0].into(),
            },
        ];
        let spec = TopN {
            col: 0,
            descending: false,
            limit: 100,
        };
        let rows = run_parallel_top_n(&segments, &program, &spec).unwrap();
        assert_eq!(rows, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
    }
}
