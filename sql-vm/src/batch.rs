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

use sql_expr::AggFunc;
pub use sql_join::JoinKind;
use sql_join::{should_emit, JoinHashTable};
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::hash::{Hash, Hasher};

/// Rows per batch that opcodes operate on at once.
pub const BATCH_SIZE: usize = 1024;

/// A runtime row value, or a `SELECT`-list literal baked into an
/// [`Opcode::LoadConst`] -- `Str` uses `Cow<'static, str>` so a codegen'd
/// `const PROGRAM` (see `codegen.rs`, #98) can hold `Cow::Borrowed("...")`
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

#[derive(Debug, PartialEq)]
pub enum VmError {
    UnknownColumn(String),
    UnknownRegister(usize),
    RegisterLengthMismatch,
    UnknownJoinTable(usize),
}

impl fmt::Display for VmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VmError::UnknownColumn(name) => write!(f, "unknown column: {name}"),
            VmError::UnknownRegister(r) => write!(f, "unknown register: {r}"),
            VmError::RegisterLengthMismatch => write!(f, "register length mismatch"),
            VmError::UnknownJoinTable(t) => write!(f, "unknown join table: {t}"),
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
        self.registers
            .get(&reg)
            .map(Vec::as_slice)
            .ok_or(VmError::UnknownRegister(reg))
    }

    pub fn execute(&mut self, batch: &Batch, program: &[Opcode]) -> Result<()> {
        for op in program {
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
        match op {
            Opcode::LoadColumn { reg, column } => {
                let values = batch
                    .columns
                    .get(column.as_ref())
                    .ok_or_else(|| VmError::UnknownColumn(column.to_string()))?;
                self.registers.insert(*reg, values.clone());
            }
            Opcode::LoadConst { reg, value } => {
                self.registers
                    .insert(*reg, vec![value.clone(); batch.num_rows]);
            }
            Opcode::Map { dst, op, a, b } => {
                let (a_vals, b_vals) = (self.register(*a)?, self.register(*b)?);
                if a_vals.len() != b_vals.len() {
                    return Err(VmError::RegisterLengthMismatch);
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
                    .register(*predicate)?
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
                        return Err(VmError::RegisterLengthMismatch);
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
                    Some(reg) => reduce_values(*func, self.register(*reg)?),
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
                    .map(|reg| self.register(*reg).map(<[Value]>::to_vec))
                    .collect::<Result<_>>()?;
                let num_rows = match key_columns.first() {
                    Some(c) => c.len(),
                    None => match aggs
                        .iter()
                        .find_map(|(_, src)| src.map(|reg| self.register(reg).map(<[Value]>::len)))
                    {
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
                            let values = self.register(*reg)?;
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
                    .map(|r| self.register(*r))
                    .collect::<Result<_>>()?;
                let payload_columns: Vec<&[Value]> = payload_cols
                    .iter()
                    .map(|r| self.register(*r))
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
                    .map(|r| self.register(*r))
                    .collect::<Result<_>>()?;
                let num_rows = key_columns.first().map_or(0, |c| c.len());
                let ht = self
                    .join_tables
                    .get(table)
                    .ok_or(VmError::UnknownJoinTable(*table))?;

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
                        return Err(VmError::RegisterLengthMismatch);
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
            Opcode::Emit { registers } => {
                // #110: borrow each register instead of `.to_vec()`-ing it
                // first -- the transpose loop below already clones every
                // cell once to build each output row, so cloning the whole
                // column again first (the previous `.to_vec()`) doubled the
                // clone cost of every surviving value for no reason.
                let cols: Vec<&[Value]> = registers
                    .iter()
                    .map(|r| self.register(*r))
                    .collect::<Result<_>>()?;
                let num_rows = cols.first().map_or(0, |c| c.len());
                let mut rows = Vec::with_capacity(num_rows);
                for row in 0..num_rows {
                    rows.push(cols.iter().map(|c| c[row].clone()).collect());
                }
                drop(cols);
                self.output.extend(rows);
            }
            // Meaningful only as loop markers interpreted by `run`.
            Opcode::Scan | Opcode::NextSegment { .. } | Opcode::Halt => {}
        }
        Ok(())
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
        assert_eq!(err, VmError::UnknownColumn("missing".into()));
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
        assert_eq!(err, VmError::RegisterLengthMismatch);
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
        assert_eq!(err, VmError::UnknownJoinTable(42));
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
