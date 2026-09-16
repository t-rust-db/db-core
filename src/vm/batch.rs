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

use crate::value::len_to_i64;
pub use crate::vm::column::{Bitmap, Column};
pub use crate::vm::join::JoinKind;
use crate::vm::join::{should_emit, JoinHashTable};
use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// Rows per batch that opcodes operate on at once.
pub const BATCH_SIZE: usize = 1024;

/// A SQL aggregate function kind: [`Opcode::Reduce`]/[`Opcode::GroupReduce`]'s
/// execution operand, shared by the batch and row VMs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFunc {
    /// `COUNT(x)` / `COUNT(*)`: number of non-null values (or rows).
    Count,
    /// `SUM(x)`: sum of the non-null values.
    Sum,
    /// `AVG(x)`: arithmetic mean of the non-null values.
    Avg,
    /// `MIN(x)`: smallest non-null value.
    Min,
    /// `MAX(x)`: largest non-null value.
    Max,
}

impl AggFunc {
    /// The canonical uppercase SQL name (`"COUNT"`, `"SUM"`, ...) -- the
    /// single source of truth [`AggFunc::from_name`] parses and
    /// `codegen::batch`'s `select_item_label`/`agg_func_name` and
    /// `parser::column`'s `ORDER BY` lowering (#131) both render back,
    /// so a SELECT-list aggregate and an `ORDER BY` reference to it
    /// produce byte-identical labels.
    pub fn name(self) -> &'static str {
        match self {
            AggFunc::Count => "COUNT",
            AggFunc::Sum => "SUM",
            AggFunc::Avg => "AVG",
            AggFunc::Min => "MIN",
            AggFunc::Max => "MAX",
        }
    }

    /// Parses a SQL aggregate name case-insensitively (the inverse of
    /// [`AggFunc::name`]); `None` for anything that isn't one of the five.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "COUNT" => Some(AggFunc::Count),
            "SUM" => Some(AggFunc::Sum),
            "AVG" => Some(AggFunc::Avg),
            "MIN" => Some(AggFunc::Min),
            "MAX" => Some(AggFunc::Max),
            _ => None,
        }
    }
}

/// A runtime row value, or a `SELECT`-list literal baked into an
/// [`Opcode::LoadConst`] -- `Str` uses `Cow<'static, str>` so an emitted
/// `const PROGRAM` (see `crate::emit::batch`, #98) can hold `Cow::Borrowed("...")`
/// literals with no heap allocation, while runtime column data (decoded
/// from arbitrary Parquet `BYTE_ARRAY` bytes) still owns its `String` via
/// `Cow::Owned`.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A 64-bit signed integer.
    Int(i64),
    /// A 64-bit IEEE float.
    Float(f64),
    /// A boolean (the result of comparisons and predicates).
    Bool(bool),
    /// A string; borrowed for baked-in literals, owned for runtime column data.
    Str(Cow<'static, str>),
    /// SQL `NULL`.
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
    /// The numeric value as `f64` (`Int` widened, `Float` as-is); `None` for
    /// `Bool`/`Str`/`Null`.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(v) => Some(*v as f64),
            Value::Float(v) => Some(*v),
            _ => None,
        }
    }
}

/// A named batch of columns (one `Arc<Vec<Value>>` per column, all the
/// same length) — the VM's input for one segment/row-group of a table.
///
/// #264: columns are `Arc`-shared, not `Vec`-owned, so cloning a `Batch`
/// (e.g. [`Segment::load`]) and loading a column into a register
/// ([`Opcode::LoadColumn`]) are both a refcount bump, not a per-cell copy.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Batch {
    /// Column values by column name; every column has exactly `num_rows` entries.
    pub columns: HashMap<String, Arc<Vec<Value>>>,
    /// Typed columns (#425/#429), keyed separately from `columns` -- a
    /// given column name lives in exactly one of the two maps.
    /// [`Opcode::LoadColumn`] loads these into a typed register
    /// (`Vm`'s `typed_registers`) without a per-row `Value` allocation;
    /// opcodes not yet ported to `Column` dispatch (#130 child 4) still
    /// read them transparently, materialized on demand.
    pub typed_columns: HashMap<String, Arc<Column>>,
    /// Number of rows in this batch (the length of every column).
    pub num_rows: usize,
}

impl Batch {
    /// An empty batch of `num_rows` rows with no columns yet.
    pub fn new(num_rows: usize) -> Self {
        Batch {
            columns: HashMap::new(),
            typed_columns: HashMap::new(),
            num_rows,
        }
    }

    /// Builder-style: adds (or replaces) the column `name` with `values`.
    pub fn with_column(mut self, name: impl Into<String>, values: Vec<Value>) -> Self {
        self.columns.insert(name.into(), Arc::new(values));
        self
    }

    /// Builder-style: adds (or replaces) the column `name` with a typed
    /// [`Column`] (#425) instead of a boxed `Vec<Value>` -- the
    /// representation [`Opcode::LoadColumn`] can load into a register
    /// without a per-row allocation (#429).
    pub fn with_typed_column(mut self, name: impl Into<String>, column: Column) -> Self {
        self.typed_columns.insert(name.into(), Arc::new(column));
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Elementwise operation applied by [`Opcode::Map`] to two registers `a` and
/// `b` (unary ops ignore `b`). Unless a variant's docs say otherwise, a `NULL`
/// operand yields `NULL`.
pub enum MapOp {
    /// `a + b` numeric addition.
    Add,
    /// `a - b` numeric subtraction.
    Sub,
    /// `a * b` numeric multiplication.
    Mul,
    /// `a / b` numeric division.
    Div,
    /// `a = b` equality comparison.
    Eq,
    /// `a <> b` inequality comparison.
    Ne,
    /// `a < b` less-than comparison.
    Lt,
    /// `a <= b` less-than-or-equal comparison.
    Le,
    /// `a > b` greater-than comparison.
    Gt,
    /// `a >= b` greater-than-or-equal comparison.
    Ge,
    /// `a AND b` logical conjunction.
    And,
    /// `a OR b` logical disjunction.
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
    /// `if b == Bool(true) { a } else { Null }` -- how an aggregate's or
    /// window function's `FILTER (WHERE ...)` clause (#67) is compiled:
    /// masking the source column to `Null` wherever the filter predicate
    /// is false lets [`Opcode::Reduce`]/[`Opcode::GroupReduce`]/
    /// [`Opcode::Window`]'s existing null-skipping do the actual
    /// filtering, with no separate opcode needed. Unlike every other
    /// binary op, this does *not* propagate `a`'s or `b`'s `Null`-ness
    /// symmetrically: only `b` (the predicate) being exactly `Bool(true)`
    /// passes `a` through.
    MaskIf,
    /// `a LIKE b` (`negated`: `NOT LIKE`) -- SQL `%`/`_` wildcard match,
    /// per [`crate::functions::like_match`]. Both operands stringify via
    /// [`Value`]'s `Display`, matching [`MapOp::Concat`]'s convention.
    Like {
        /// `true` for `NOT LIKE`.
        negated: bool,
    },
    /// `a GLOB b` (`negated`: `NOT GLOB`) -- Unix glob match (`*`/`?`/
    /// `[...]`), per [`crate::functions::glob_match`].
    Glob {
        /// `true` for `NOT GLOB`.
        negated: bool,
    },
}

/// Window functions supported by [`Opcode::Window`] -- `Sum`/`Avg`/`Count`
/// here are the running/whole-partition `OVER` forms, distinct from
/// [`AggFunc`]'s flat/`GROUP BY` reductions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowFunc {
    /// `ROW_NUMBER()`: 1-based position of the row within its sorted partition.
    RowNumber,
    /// `RANK()`: 1-based rank with gaps after ties.
    Rank,
    /// `DENSE_RANK()`: 1-based rank without gaps after ties.
    DenseRank,
    /// `LAG(arg, offset)`: `arg` from the row `offset` places earlier in the
    /// partition, `NULL` if none.
    Lag,
    /// `LEAD(arg, offset)`: `arg` from the row `offset` places later in the
    /// partition, `NULL` if none.
    Lead,
    /// `FIRST_VALUE(arg)`: `arg` from the first row of the partition.
    FirstValue,
    /// `LAST_VALUE(arg)`: `arg` from the last row of the partition.
    LastValue,
    /// `SUM(arg) OVER (...)`: running (with `ORDER BY`) or whole-partition sum.
    Sum,
    /// `AVG(arg) OVER (...)`: running (with `ORDER BY`) or whole-partition mean.
    Avg,
    /// `COUNT(arg) OVER (...)`: running or whole-partition count of non-null
    /// `arg` values (or of rows when `arg` is `None`).
    Count,
}

/// One `GROUP BY`/aggregate part of an emitted row, in emit order -- the
/// per-segment `GroupReduce` output's shape, which [`Opcode::Combine`]
/// needs to merge partial aggregates across segments (`Sum`/`Count` add,
/// `Min`/`Max` compare, `Avg` divides its `(sum_index, count_index)` pair
/// at the very end). Pure planning metadata, storage-agnostic. `Copy` so
/// an AOT-emitted `const PROGRAM` can hold a `Cow::Borrowed(&[AggPart])`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggPart {
    /// A `GROUP BY` key column: identifies the group, never merged.
    GroupKey,
    /// A partial `SUM`: merged by adding across segments.
    Sum,
    /// A partial `COUNT`: merged by adding across segments.
    Count,
    /// A partial `MIN`: merged by keeping the smaller value.
    Min,
    /// A partial `MAX`: merged by keeping the larger value.
    Max,
    /// `(sum_index, count_index)` into the emitted row, combined at the end.
    Avg(usize, usize),
}

#[derive(Debug, Clone, PartialEq)]
/// One instruction of the batch VM. Register operands are indices into the
/// VM's register map; each register holds one column of the current batch.
pub enum Opcode {
    /// Load a named column from the current batch into a register.
    LoadColumn {
        /// Destination register.
        reg: usize,
        /// Name of the column to load from the batch.
        column: Cow<'static, str>,
    },
    /// Broadcast a constant value to every row of the current batch into a
    /// register.
    LoadConst {
        /// Destination register.
        reg: usize,
        /// The value broadcast to every row.
        value: Value,
    },
    /// Apply a binary op elementwise: `registers[dst] = op(registers[a], registers[b])`.
    Map {
        /// Destination register.
        dst: usize,
        /// The operation to apply.
        op: MapOp,
        /// Left (or sole, for unary ops) operand register.
        a: usize,
        /// Right operand register; ignored by unary ops.
        b: usize,
    },
    /// Keep only the rows where `predicate` register holds `Value::Bool(true)`,
    /// applied to every currently-live register (in place).
    Filter {
        /// Register holding one `Bool` per row; `true` keeps the row.
        predicate: usize,
    },
    /// Aggregate a whole register down to a single value (skipping nulls).
    /// `COUNT` counts non-null values, or all rows when `src` is `None`
    /// (`COUNT(*)`).
    Reduce {
        /// The aggregate to compute.
        func: AggFunc,
        /// Source register, or `None` for `COUNT(*)`.
        src: Option<usize>,
        /// Destination register (one value).
        dst: usize,
    },
    /// Hash-aggregate: partition rows by the tuple of values in `group_by`
    /// registers, then reduce each `(func, src)` pair per group. Writes one
    /// row per distinct group into `group_by` registers (deduplicated) plus
    /// one output register per aggregate, in `aggs` order.
    GroupReduce {
        /// Registers whose value tuple identifies a group.
        group_by: Cow<'static, [usize]>,
        /// `(aggregate, source register)` pairs; `None` source is `COUNT(*)`.
        aggs: Cow<'static, [(AggFunc, Option<usize>)]>,
        /// Destination register for each entry of `aggs`, in order.
        agg_dst: Cow<'static, [usize]>,
    },
    /// Build a hash table for an equi-join from the current live registers:
    /// `key_cols` (compound key, NULL-safe -- see [`JoinKey`]) and
    /// `payload_cols` (columns carried through to the probe side), keyed by
    /// `table` so a later [`Opcode::HashProbe`] can find it.
    HashBuild {
        /// Registers forming the compound join key.
        key_cols: Cow<'static, [usize]>,
        /// Registers carried through to the probe side as payload.
        payload_cols: Cow<'static, [usize]>,
        /// Identifier a later [`Opcode::HashProbe`] uses to find this table.
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
        /// Probe-side registers forming the compound join key.
        key_cols: Cow<'static, [usize]>,
        /// Identifier of the table built by [`Opcode::HashBuild`].
        table: usize,
        /// Destination register for each build-side payload column, in order.
        payload_dst: Cow<'static, [usize]>,
        /// Which rows to emit for matches/non-matches.
        kind: JoinKind,
    },
    /// #441: `Opcode::HashProbe` immediately followed by an
    /// `Opcode::GroupReduce` over the joined row, fused into one pass so a
    /// join whose output feeds only an aggregate never materializes a
    /// joined row per match. `codegen::batch::compile_join_impl` emits
    /// this instead of `HashProbe` exactly when the join body is `GROUP
    /// BY`/aggregate-only (no `Map`/`Filter`/`Window` between the probe
    /// and the reduce) -- every other join shape still goes through the
    /// unfused `HashProbe` + a separate `GroupReduce`.
    ///
    /// Each `group_by`/`aggs` source is a [`ValueSource`]: a probe-side
    /// register (already loaded, like `HashProbe`'s `key_cols`) or a
    /// build-side payload column index (in `HashBuild`'s `payload_cols`
    /// order -- NULL for an unmatched probe row, matching `HashProbe`'s
    /// own NULL-fill for `payload_dst`). Grouping/hashing reuses
    /// `GroupReduce`'s semantics exactly (`Null` groups with `Null`); join
    /// matching reuses `HashProbe`'s (`kind`'s [`should_emit`] rule, NULL
    /// keys never matching).
    HashProbeGroupReduce {
        /// Probe-side registers forming the compound join key.
        key_cols: Cow<'static, [usize]>,
        /// Identifier of the table built by [`Opcode::HashBuild`].
        table: usize,
        /// Which rows contribute for matches/non-matches.
        kind: JoinKind,
        /// `(value source, destination register)` pairs identifying a
        /// group, mirroring [`Opcode::GroupReduce`]'s dual-purpose
        /// `group_by` (the destination is written with one value per
        /// distinct group, in discovery order).
        group_by: Cow<'static, [(ValueSource, usize)]>,
        /// `(aggregate, source)` pairs; `None` source is `COUNT(*)`.
        aggs: Cow<'static, [(AggFunc, Option<ValueSource>)]>,
        /// Destination register for each entry of `aggs`, in order.
        agg_dst: Cow<'static, [usize]>,
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
        /// The window function to compute.
        func: WindowFunc,
        /// Value register for functions that take an argument.
        arg: Option<usize>,
        /// Row shift for `Lag`/`Lead` (default 1).
        offset: Option<i64>,
        /// Registers whose value tuple identifies a partition (empty = one).
        partition_by: Cow<'static, [usize]>,
        /// `(register, descending)` sort keys within each partition.
        order_by: Cow<'static, [(usize, bool)]>,
        /// Destination register (one value per row).
        dst: usize,
    },
    /// Marks the top of the per-segment loop; a no-op on its own (the
    /// current batch is already loaded by [`Vm::run`]).
    Scan,
    /// Names where a cross-mode join's build side materializes its
    /// [`Batch`] from (ADR 0024, #382/#384) -- a no-op in the per-segment
    /// [`Vm::step`], like [`Opcode::Scan`]/[`Opcode::Combine`]; resolving a
    /// [`ScanSource`] into an actual `Batch` is
    /// [`crate::vm::engine`]'s job (#385), not this executor's, since only
    /// the orchestration layer above a single segment's batch is allowed to
    /// depend on how a row table or stream engine is reached.
    ScanSource(ScanSource),
    /// Append the current values of `registers` (transposed row-major) to
    /// the VM's output.
    Emit {
        /// Registers forming the output columns, in order.
        registers: Cow<'static, [usize]>,
    },
    /// If the source has another segment, load it and jump back to
    /// `loop_start` (the instruction index right after [`Opcode::Scan`]);
    /// otherwise fall through.
    NextSegment {
        /// Instruction index to jump back to when another segment exists.
        loop_start: usize,
    },
    /// Stop execution.
    Halt,
    /// Cross-segment merge step of a planned flat program (ADR 0007,
    /// db-core#48): merges per-segment partial aggregates by group key
    /// (`agg_parts`/`num_group_keys`), then finalizes them (e.g. `Avg` =
    /// merged sum / merged count), then deduplicates if `distinct`.
    /// Mirrors DuckDB's `Combine` (merge thread-local partial states) and
    /// `Finalize` (compute the final value from the merged state) as one
    /// opcode -- db-core's `merge_rows`/`finalize_row` are exactly those
    /// two steps back to back with no observable boundary between them.
    /// A *barrier*: it needs every segment's output, so the per-segment
    /// [`Vm`] treats it as a no-op control opcode (like [`Opcode::Scan`]/
    /// [`Opcode::Halt`]), and [`crate::vm::engine::run`] applies it once
    /// over the concatenated output before any [`Opcode::Sort`]/
    /// [`Opcode::Limit`] that follows. Always the first of a program's
    /// trailing sequential-phase opcodes, if any are present at all --
    /// `Sort`/`Limit` alone, with no `Combine`, never appear (see
    /// [`Program::split_finalize`]).
    Combine {
        /// Shape of each emitted row, for merging partial aggregates.
        agg_parts: Cow<'static, [AggPart]>,
        /// How many leading emitted columns are `GROUP BY` keys.
        num_group_keys: usize,
        /// Deduplicate identical output rows (`SELECT DISTINCT`).
        distinct: bool,
    },
    /// Final `ORDER BY` over the whole (already merged) result: sorts by
    /// output column `col`, `descending` or ascending. A sequential-phase
    /// opcode, like [`Opcode::Combine`]/[`Opcode::Limit`] -- see
    /// [`Program::split_finalize`].
    Sort {
        /// Output column index to sort by.
        col: usize,
        /// `true` for descending, `false` for ascending.
        descending: bool,
    },
    /// Final `LIMIT`: keep only the first `n` rows of the whole
    /// (already merged, and sorted if [`Opcode::Sort`] preceded this)
    /// result. A sequential-phase opcode -- see [`Program::split_finalize`].
    Limit {
        /// Maximum number of rows to keep.
        n: usize,
    },
    /// Elementwise scalar function call: `registers[dst] = functions::call(
    /// name, [registers[a] for a in args])`, per row. Dispatches into
    /// `crate::functions`'s feature-free registry (ADR 0011) -- `codegen::
    /// batch` never duplicates a scalar function's body -- via a small
    /// [`Value`]<->[`crate::value::Value`] conversion (#307). An unknown
    /// name/arity or a `crate::value::Value::Blob` result (no `Value::Blob`
    /// variant here -- batch has never needed one) both yield `Value::Null`
    /// rather than an error, the same convention `compile_expr` already
    /// uses for every out-of-subset shape.
    Call {
        /// Destination register.
        dst: usize,
        /// Function name, dispatched by `functions::call`.
        name: Cow<'static, str>,
        /// Argument registers, in call order.
        args: Cow<'static, [usize]>,
    },
}

/// Where [`Opcode::ScanSource`] materializes a cross-mode join's build side
/// from (ADR 0024, #382/#384) -- a closed enum, not a trait object, so
/// every source's row/batch/stream origin stays visible and exhaustively
/// matchable at the opcode level, instead of being erased behind
/// `Box<dyn Segment>` once past construction. `vm::batch` cannot depend on
/// `engine`'s concrete `RowEngine`/`StreamEngine` types (that would invert
/// the layering ADR-0001 fixes), so [`ScanSource::RowTable`] and
/// [`ScanSource::Stream`] name their source by value (table name, stream
/// handle) rather than by reference -- the same indirection
/// [`Opcode::HashBuild`]/[`Opcode::HashProbe`]'s `table: usize` already
/// uses to point at a resource resolved elsewhere. Resolving a `ScanSource`
/// into an actual [`Batch`] is [`crate::vm::engine`]'s job (#385); this
/// type only names the resource.
#[derive(Debug, Clone, PartialEq)]
pub enum ScanSource {
    /// A SQLite table read through `engine::row`, by name.
    RowTable {
        /// The table's name.
        table: Cow<'static, str>,
        /// Columns to materialize, in the build program's register order.
        columns: Cow<'static, [Cow<'static, str>]>,
    },
    /// A windowed (or unbounded) set of stream segments, identified by an
    /// opaque handle the executor resolves to a concrete stream engine --
    /// mirrors `vm::stream::Scope`'s windowing (`SINCE`/`UNTIL`). `vm::batch`
    /// builds without `vm-stream` enabled (`vm-stream` depends on
    /// `vm-batch`, never the reverse), so this variant cannot name
    /// `vm::stream::Scope` directly; `scope` is only present when
    /// `vm-stream` is, and its resolver (#385, `vm::engine`, which does
    /// depend on `vm::stream`) is responsible for interpreting it.
    Stream {
        /// Opaque identifier for which stream engine/table to read;
        /// meaningful only to whatever resolves this opcode.
        handle: usize,
        /// Columns to materialize, in the build program's register order.
        columns: Cow<'static, [Cow<'static, str>]>,
        /// The windowing scope, if any -- required for stream-to-stream
        /// joins (ADR 0022), since neither side is bounded by construction.
        #[cfg(feature = "vm-stream")]
        scope: Option<crate::vm::stream::Scope>,
    },
    /// An already-materialized batch -- no scan needed (e.g. a
    /// table-to-table batch join's build side, or a literal input).
    InMemory(Batch),
}

impl Opcode {
    /// The opcode's variant name, used as `VmError`'s runtime-error context
    /// (the execution-time equivalent of `Span` for parse errors -- there's
    /// no source text left at execution time, but there's always a specific
    /// instruction that failed).
    pub fn name(&self) -> &'static str {
        match self {
            Opcode::LoadColumn { .. } => "LoadColumn",
            Opcode::LoadConst { .. } => "LoadConst",
            Opcode::Map { .. } => "Map",
            Opcode::Filter { .. } => "Filter",
            Opcode::Reduce { .. } => "Reduce",
            Opcode::GroupReduce { .. } => "GroupReduce",
            Opcode::HashBuild { .. } => "HashBuild",
            Opcode::HashProbe { .. } => "HashProbe",
            Opcode::HashProbeGroupReduce { .. } => "HashProbeGroupReduce",
            Opcode::Window { .. } => "Window",
            Opcode::Scan => "Scan",
            Opcode::ScanSource { .. } => "ScanSource",
            Opcode::Emit { .. } => "Emit",
            Opcode::NextSegment { .. } => "NextSegment",
            Opcode::Halt => "Halt",
            Opcode::Combine { .. } => "Combine",
            Opcode::Sort { .. } => "Sort",
            Opcode::Limit { .. } => "Limit",
            Opcode::Call { .. } => "Call",
        }
    }
}

/// [`Value`] -> [`crate::value::Value`], for calling into `functions::call`
/// (#307). `Bool` has no row-value equivalent -- SQLite's own storage
/// classes have no boolean -- so it becomes `Integer(0/1)`, the same
/// coercion `CAST(bool AS INTEGER)` would give.
fn to_scalar_value(v: &Value) -> crate::value::Value {
    match v {
        Value::Int(i) => crate::value::Value::Integer(*i),
        Value::Float(f) => crate::value::Value::Real(*f),
        Value::Bool(b) => crate::value::Value::Integer(i64::from(*b)),
        Value::Str(s) => crate::value::Value::Text(s.as_ref().into()),
        Value::Null => crate::value::Value::Null,
    }
}

/// [`crate::value::Value`] -> [`Value`], the reverse of
/// [`to_scalar_value`]. `Blob` has no `Value` equivalent here (batch has
/// never needed one) and becomes `Null` rather than erroring.
fn from_scalar_value(v: crate::value::Value) -> Value {
    match v {
        crate::value::Value::Null | crate::value::Value::Blob(_) => Value::Null,
        crate::value::Value::Integer(i) => Value::Int(i),
        crate::value::Value::Real(f) => Value::Float(f),
        crate::value::Value::Text(s) => Value::Str(s.to_string().into()),
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
    /// The instruction to execute.
    pub opcode: Opcode,
    /// Optional human-readable note shown in `EXPLAIN` listings.
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

/// A query's result, stored column-major (#436): `columns()[c][r]` is
/// output column `c`'s value at row `r`. Replaces the row-major `Vec<Vec<Value>>`
/// every batch-engine execution entry point (`Opcode::Emit`, [`run_parallel`],
/// `vm::engine::run`/`run_join`/`finalize`) used to build and pass around --
/// materializing one `Vec` per row was measured at 96% of a 5M-row filter's
/// time and RSS, purely container overhead (a `Vec` header plus a malloc'd
/// buffer per row, for as little as one `i64`/`f64` of payload). `Opcode::Emit`
/// now moves or extends whole columns; concatenating segments
/// ([`QueryOutput::extend`]) is an `O(rows)` `Vec::extend` per column, not an
/// allocation per row.
///
/// Row-shaped consumers (a CLI printer, `ORDER BY`'s top-N heap, `GROUP BY`'s
/// cross-segment merge) still get row-major data where they need it, via
/// [`QueryOutput::into_rows`] -- but only over however many rows actually
/// reach that stage (already `LIMIT`ed, or already collapsed to a handful of
/// groups), not the full survivor count a plain filter/projection produces.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QueryOutput {
    columns: Vec<Vec<Value>>,
}

impl QueryOutput {
    /// Wraps already column-major data (`columns[c][r]`).
    pub fn new(columns: Vec<Vec<Value>>) -> Self {
        Self { columns }
    }

    /// Transposes row-major data into columns, inferring the column count
    /// from the first row (0 for an empty `rows`).
    pub fn from_rows(rows: Vec<Vec<Value>>) -> Self {
        let num_columns = rows.first().map_or(0, Vec::len);
        let mut columns: Vec<Vec<Value>> = (0..num_columns)
            .map(|_| Vec::with_capacity(rows.len()))
            .collect();
        for row in rows {
            for (c, value) in row.into_iter().enumerate() {
                if let Some(column) = columns.get_mut(c) {
                    column.push(value);
                }
            }
        }
        Self { columns }
    }

    /// Number of output columns.
    #[must_use]
    pub fn num_columns(&self) -> usize {
        self.columns.len()
    }

    /// Number of rows -- every column's length (they're always equal).
    #[must_use]
    pub fn num_rows(&self) -> usize {
        self.columns.first().map_or(0, Vec::len)
    }

    /// True when there are no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.num_rows() == 0
    }

    /// Alias for [`Self::num_rows`] -- callers migrating from the old
    /// row-major `Vec<Vec<Value>>` (#436) that only ever asked its length.
    #[must_use]
    pub fn len(&self) -> usize {
        self.num_rows()
    }

    /// The output, column-major.
    #[must_use]
    pub fn columns(&self) -> &[Vec<Value>] {
        &self.columns
    }

    /// The output, column-major, moved out.
    #[must_use]
    pub fn into_columns(self) -> Vec<Vec<Value>> {
        self.columns
    }

    /// Transposes to row-major -- one `Vec<Value>` allocation per row.
    /// Only meant for however many rows actually need to be row-shaped
    /// (see the type's own doc comment), not a full survivor set.
    #[must_use]
    pub fn into_rows(self) -> Vec<Vec<Value>> {
        let num_rows = self.num_rows();
        if self.columns.is_empty() {
            return Vec::new();
        }
        let mut column_iters: Vec<std::vec::IntoIter<Value>> = self
            .columns
            .into_iter()
            .map(IntoIterator::into_iter)
            .collect();
        (0..num_rows)
            .map(|_| {
                column_iters
                    .iter_mut()
                    .map(|it| it.next().unwrap_or(Value::Null))
                    .collect()
            })
            .collect()
    }

    /// Appends `other`'s rows onto this output's columns, column-wise
    /// (`Vec::extend`, no per-row allocation). The first call onto an
    /// empty (column-count-0) output just adopts `other`'s columns.
    pub fn extend(&mut self, other: QueryOutput) {
        if self.columns.is_empty() {
            self.columns = other.columns;
            return;
        }
        for (into, from) in self.columns.iter_mut().zip(other.columns) {
            into.extend(from);
        }
    }

    /// Truncates every column to at most `n` rows.
    pub fn truncate(&mut self, n: usize) {
        for column in &mut self.columns {
            column.truncate(n);
        }
    }
}

/// Row-shape equality against the pre-#436 row-major representation --
/// lets a caller (or a test asserting an expected result) compare a
/// [`QueryOutput`] to a `vec![vec![...], ...]` literal directly, without
/// spelling out a transpose. Transposes `self` (clones), so this is for
/// convenience/tests, not a hot path.
impl PartialEq<Vec<Vec<Value>>> for QueryOutput {
    fn eq(&self, other: &Vec<Vec<Value>>) -> bool {
        self.clone().into_rows() == *other
    }
}

impl From<Vec<Vec<Value>>> for QueryOutput {
    fn from(rows: Vec<Vec<Value>>) -> Self {
        Self::from_rows(rows)
    }
}

impl From<QueryOutput> for Vec<Vec<Value>> {
    fn from(output: QueryOutput) -> Self {
        output.into_rows()
    }
}

/// A linear program of [`Instruction`]s, mirroring sqlite-rs's
/// `vdbe::program::Program`. Everything the executor needs is *in* the
/// instruction stream: the columns to load are the [`Opcode::LoadColumn`]
/// operands, and the cross-segment merge/sort/limit metadata is the
/// trailing `Combine`/`Sort`/`Limit` sequence -- no sidecar plan struct
/// (ADR 0007, db-core#48).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Program {
    /// The instructions, executed in order from index 0.
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

    /// Splits off a trailing `Combine [Sort] [Limit]` sequence (db-core#48):
    /// `(body opcodes, the Combine, the Sort, the Limit)`. Tries the
    /// longest shape first (`Combine, Sort, Limit`) so a genuine 3-opcode
    /// tail is never mistaken for a shorter one that happens to end in an
    /// opcode of the same kind. A program with no trailing `Combine` at
    /// all -- `Sort`/`Limit` alone never appear, since [`super::super::codegen::batch::compile`]
    /// always emits `Combine` when it emits either -- returns every
    /// opcode and `(None, None, None)`, and executes as a plain
    /// per-segment concatenation.
    pub fn split_finalize(
        &self,
    ) -> (
        Vec<Opcode>,
        Option<&Opcode>,
        Option<&Opcode>,
        Option<&Opcode>,
    ) {
        let ops = &self.instructions;
        let n = ops.len();
        for tail_len in [3usize, 2, 1] {
            let Some(split_at) = n.checked_sub(tail_len) else {
                continue;
            };
            let Some(tail) = ops.get(split_at..) else {
                continue;
            };
            let opcodes: Vec<&Opcode> = tail.iter().map(|i| &i.opcode).collect();
            let matched = match opcodes.as_slice() {
                [combine @ Opcode::Combine { .. }] => Some((Some(*combine), None, None)),
                [combine @ Opcode::Combine { .. }, sort @ Opcode::Sort { .. }] => {
                    Some((Some(*combine), Some(*sort), None))
                }
                [combine @ Opcode::Combine { .. }, limit @ Opcode::Limit { .. }] => {
                    Some((Some(*combine), None, Some(*limit)))
                }
                [combine @ Opcode::Combine { .. }, sort @ Opcode::Sort { .. }, limit @ Opcode::Limit { .. }] => {
                    Some((Some(*combine), Some(*sort), Some(*limit)))
                }
                _ => None,
            };
            if let Some((combine, sort, limit)) = matched {
                let body = ops
                    .get(..split_at)
                    .map(|b| b.iter().map(|i| i.opcode.clone()).collect())
                    .unwrap_or_default();
                return (body, combine, sort, limit);
            }
        }
        (self.opcodes().cloned().collect(), None, None, None)
    }
}

/// Supplies successive batches (row-group segments) of a table to
/// [`Vm::run`].
pub trait Source {
    /// The next batch, or `None` once the source is exhausted.
    fn next_batch(&mut self) -> Option<Batch>;
}

/// One independently-loadable unit of work for [`run_parallel`] — typically
/// a single row group's worth of columns.
pub trait Segment: Send + Sync {
    /// Loads this segment's columns into a [`Batch`]. Fallible since #272:
    /// a segment may itself run a program (the probe side of a join, see
    /// `vm::engine::run_join_segments`) or decode storage, and either can
    /// fail -- a typed error here reaches the caller instead of a panic or
    /// a silently empty batch. Returns `Arc<Batch>` (#264) so an
    /// implementor backed by an already-materialized `Batch` can hand it
    /// out with a refcount bump instead of a deep copy.
    fn load(&self) -> Result<Arc<Batch>>;
}

/// Dynamically hands out segment indices to a fixed pool of worker threads:
/// each thread loops fetch-adding a shared counter to claim its next
/// segment until the counter runs past `len`, so a thread that finishes an
/// expensive segment immediately pulls the next unclaimed one rather than
/// sitting on a statically pre-assigned share -- the morsel-driven property
/// `run_parallel`/`run_parallel_top_n`'s doc comments require, without a
/// work-stealing dependency.
fn run_morsels<I: Sync, T: Send>(items: &[I], f: impl Fn(&I) -> T + Sync) -> Vec<T> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    let len = items.len();
    if len == 0 {
        return Vec::new();
    }

    let num_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(len);
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<(usize, T)>> = Mutex::new(Vec::with_capacity(len));

    std::thread::scope(|scope| {
        for _ in 0..num_threads {
            scope.spawn(|| loop {
                let idx = next.fetch_add(1, Ordering::Relaxed);
                let Some(item) = items.get(idx) else {
                    break;
                };
                let result = f(item);
                // Poison recovery is unreachable in practice: `thread::scope`
                // re-raises any worker panic when the scope closes, so a
                // poisoned lock never yields a short result set silently.
                results
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((idx, result));
            });
        }
    });

    let mut results = results
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    results.sort_unstable_by_key(|(idx, _)| *idx);
    results.into_iter().map(|(_, result)| result).collect()
}

/// Run `program` (a flat, non-looping instruction list ending in
/// [`Opcode::Emit`]) against every segment in parallel (morsel-driven: a
/// fixed thread pool dynamically pulls one segment per task off a shared
/// counter, see [`run_morsels`]), then concatenate the emitted rows in
/// segment order.
///
/// `GroupReduce`/`Reduce` results are per-segment only — merging partial
/// aggregates across segments is not performed here.
pub fn run_parallel<S: Segment>(segments: &[S], program: &[Opcode]) -> Result<QueryOutput> {
    let per_segment: Vec<Result<QueryOutput>> = run_morsels(segments, |segment| {
        let batch = segment.load()?;
        let mut vm = Vm::new();
        vm.execute(&batch, program)?;
        Ok(std::mem::take(&mut vm.output))
    });

    // #436: column-wise `Vec::extend` per segment, not a per-row rebuild.
    let mut all = QueryOutput::default();
    for output in per_segment {
        all.extend(output?);
    }
    Ok(all)
}

/// `ORDER BY <col> [ASC|DESC] LIMIT <limit>` spec for [`run_parallel_top_n`]:
/// which emitted output column to order by, direction, and how many rows to
/// keep.
#[derive(Debug, Clone, Copy)]
pub struct TopN {
    /// Index of the emitted output column to order by.
    pub col: usize,
    /// Sort descending when `true`, ascending otherwise.
    pub descending: bool,
    /// Number of rows to keep.
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
                // #266: compare `Str`/`Str` directly -- `Value`'s `Display`
                // prints a `Str`'s contents verbatim (no quoting), so this
                // is byte-identical to the `to_string()` fallback below
                // without the two allocations.
                _ => match (a, b) {
                    (Value::Str(x), Value::Str(y)) => x.cmp(y),
                    _ => a.to_string().cmp(&b.to_string()),
                },
            };
            // ORDER BY direction flips the comparison, not the sort itself.
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
    #[allow(
        clippy::indexing_slicing,
        reason = "`col` is the ORDER BY register codegen resolved against the row width; `Ord` has no error path"
    )]
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

    let mut heap: BinaryHeap<TopNItem> =
        BinaryHeap::with_capacity(spec.limit.min(rows.len()).saturating_add(1));
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
pub fn run_parallel_top_n<S: Segment>(
    segments: &[S],
    program: &[Opcode],
    spec: &TopN,
) -> Result<QueryOutput> {
    // #436: `top_n_reduce`'s heap is inherently row-shaped (each candidate
    // is compared by one `ORDER BY` column against the current worst kept
    // row), but it only ever holds `spec.limit` rows at a time -- row-major
    // here is not the container-overhead problem `Opcode::Emit` had, since
    // the row count is bounded by the query's own `LIMIT`, not by how many
    // rows survived the scan.
    let per_segment: Vec<Result<Vec<Vec<Value>>>> = run_morsels(segments, |segment| {
        let batch = segment.load()?;
        let mut vm = Vm::new();
        vm.execute(&batch, program)?;
        Ok(top_n_reduce(
            std::mem::take(&mut vm.output).into_rows(),
            spec,
        ))
    });

    let mut all = Vec::new();
    for rows in per_segment {
        all.extend(rows?);
    }
    Ok(QueryOutput::from_rows(top_n_reduce(all, spec)))
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
    /// [`Opcode::LoadColumn`] named a column the batch doesn't have.
    UnknownColumn {
        /// Name of the [`Opcode`] variant that failed.
        opcode: &'static str,
        /// The column name that was not found.
        column: String,
    },
    /// An opcode read a register that has not been written.
    UnknownRegister {
        /// Name of the [`Opcode`] variant that failed.
        opcode: &'static str,
        /// The register index that was not found.
        register: usize,
    },
    /// Two registers combined by one opcode hold different numbers of rows.
    RegisterLengthMismatch {
        /// Name of the [`Opcode`] variant that failed.
        opcode: &'static str,
    },
    /// [`Opcode::HashProbe`] referenced a table no [`Opcode::HashBuild`] built.
    UnknownJoinTable {
        /// Name of the [`Opcode`] variant that failed.
        opcode: &'static str,
        /// The join table identifier that was not found.
        table: usize,
    },
    /// `Vm::step` was about to execute past [`MAX_STEPS`] instructions.
    StepLimitExceeded {
        /// Name of the [`Opcode`] variant that failed.
        opcode: &'static str,
        /// The step limit that was exceeded ([`MAX_STEPS`]).
        limit: usize,
    },
    /// An opcode named an operation its kernel has no dispatch for -- a
    /// planner bug, surfaced to the caller as an error rather than a
    /// panic mid-query.
    UnsupportedOp {
        /// Name of the [`Opcode`] variant that failed.
        opcode: &'static str,
        /// The operation (e.g. a `MapOp` or function name) with no dispatch.
        op: String,
    },
    /// An [`Opcode::Window`] for a function that takes an argument
    /// (`Lag`/`Lead`/`FirstValue`/`LastValue`) came with `arg: None` -- a
    /// planner bug, surfaced as an error rather than a panic (db-core#231).
    MissingWindowArgument {
        /// Name of the [`Opcode`] variant that failed.
        opcode: &'static str,
        /// The window function that needed an argument register.
        func: WindowFunc,
    },
    /// An opcode's operands break an invariant the planner guarantees
    /// (`HashBuild` with no key columns, `Emit` with no registers, a join
    /// payload narrower than its destinations, a non-numeric partial
    /// aggregate). A planner bug, surfaced as an error instead of a
    /// plausible-looking wrong result (db-core#232).
    MalformedProgram {
        /// Name of the [`Opcode`] variant (or engine phase) that failed.
        opcode: &'static str,
        /// Which invariant failed.
        reason: String,
    },
    /// A [`Segment::load`] failed for a reason outside the VM -- a storage
    /// decode error, a row group that does not exist. Storage backends
    /// return this instead of a panic or a silently NULL-filled column
    /// (t-rust-db/column-rs#27).
    SegmentLoad {
        /// What failed, in the backend's own words (file, column, cause).
        reason: String,
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
            VmError::UnsupportedOp { opcode, op } => {
                write!(f, "{opcode}: no dispatch for {op}")
            }
            VmError::MissingWindowArgument { opcode, func } => {
                write!(f, "{opcode}: {func:?} requires an argument register")
            }
            VmError::MalformedProgram { opcode, reason } => {
                write!(f, "{opcode}: malformed program: {reason}")
            }
            VmError::SegmentLoad { reason } => write!(f, "segment load failed: {reason}"),
        }
    }
}

impl std::error::Error for VmError {}

/// Result type of every fallible VM operation, erroring with [`VmError`].
pub type Result<T> = std::result::Result<T, VmError>;

/// #263: `Opcode::GroupReduce`'s key -- `PartialEq`/`Eq` are derived
/// (plain [`Value`] equality, where `Null == Null`), matching `GROUP BY`
/// semantics (as distinct from a join key, where NULL never matches
/// anything, including another NULL -- see [`join_keys_match`]). `Hash`
/// uses a variant-tagged scheme (shared with joins via
/// [`hash_group_value`]) so `Int(1)` and `Str("1")` never collide.
#[derive(Debug, Clone, PartialEq)]
struct GroupKey(Vec<Value>);

// `Value::Float` isn't `Eq` (NaN), so `Eq` can't be derived -- but the
// `Hash` impl below never inspects a float's ordering, only its bit
// pattern, so `Eq`'s reflexivity requirement still holds in practice.
impl Eq for GroupKey {}

impl Hash for GroupKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        for value in &self.0 {
            hash_group_value(value, state);
        }
    }
}

/// Variant-tagged hash for a single [`Value`], shared by [`GroupKey`]'s
/// `Hash` impl and `Opcode::GroupReduce`'s probe-before-insert hot loop
/// (#439) -- both need the exact same scheme so a probe hash always
/// matches the hash the group's key was originally inserted under.
fn hash_group_value<H: Hasher>(value: &Value, state: &mut H) {
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

/// #440: column-wise row hashing for `GroupReduce`/`HashBuild`/`HashProbe` --
/// one [`DefaultHasher`] per row, folded column by column (outer loop over
/// `columns`, inner loop over rows) rather than the row-major "gather a
/// `Vec<Value>` key, then hash it" shape those opcodes used before. Never
/// materializes a per-row key: the row's contribution to its own hasher is
/// read straight out of each column in turn.
#[allow(
    clippy::indexing_slicing,
    reason = "callers pass `row`/`physical(row)` drawn from that same batch's `0..num_rows`, and every key/build column here holds exactly `num_rows` values (checked by the caller via `RegisterLengthMismatch` before this runs)"
)]
fn hash_columns_by_row(
    columns: &[&[Value]],
    num_rows: usize,
    physical: impl Fn(usize) -> usize,
) -> Vec<u64> {
    let mut hashers: Vec<DefaultHasher> = (0..num_rows).map(|_| DefaultHasher::new()).collect();
    for column in columns {
        for (row, hasher) in hashers.iter_mut().enumerate() {
            hash_group_value(&column[physical(row)], hasher);
        }
    }
    hashers.into_iter().map(|h| h.finish()).collect()
}

/// SQL join-key equality between a probe row and an already-built row's
/// key columns: NULL never matches anything, including another NULL (as
/// distinct from [`GroupKey`]'s `GROUP BY` semantics, where `Null ==
/// Null`) -- see #440. `build_keys[i][build_row]` is the build-side value
/// of key column `i`; `probe_columns[i][probe_row]` is its probe-side
/// counterpart.
#[allow(
    clippy::indexing_slicing,
    reason = "`probe_row` is in range for every `probe_columns` entry (same invariant as `hash_columns_by_row`'s caller); `build_row` came from `BuildTable::index`, which only ever stores row numbers `push`ed alongside `build_keys` in `Opcode::HashBuild`, so it is in range for every one of `build_keys`'s equal-length columns"
)]
fn join_keys_match(
    probe_columns: &[&[Value]],
    probe_row: usize,
    build_keys: &[Vec<Value>],
    build_row: usize,
) -> bool {
    probe_columns.iter().enumerate().all(|(i, c)| {
        let probe_value = &c[probe_row];
        let build_value = &build_keys[i][build_row];
        !matches!(probe_value, Value::Null)
            && !matches!(build_value, Value::Null)
            && probe_value == build_value
    })
}

/// One `Opcode::HashBuild`'s table: key and payload columns stored
/// column-major (one `Vec<Value>` allocation per column, not per row --
/// #440), plus a flat `index` mapping each build row's hash to that row's
/// position in `keys`/`payload`. `index`'s value is a plain row number,
/// not an owned key -- collisions are resolved by [`join_keys_match`]
/// against `keys`, not by the table's own `Eq`.
struct BuildTable {
    index: JoinHashTable<u64, usize>,
    keys: Vec<Vec<Value>>,
    payload: Vec<Vec<Value>>,
}

impl fmt::Debug for BuildTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuildTable")
            .field("len", &self.index.len())
            .field("capacity", &self.index.capacity())
            .finish()
    }
}

/// #441: where [`Opcode::HashProbeGroupReduce`] reads one `GROUP BY` key
/// component or aggregate input from, for a given matched (or unmatched)
/// row -- a probe-side register (already loaded, like `HashProbe`'s
/// `key_cols`) or a build-side payload column (in `HashBuild`'s
/// `payload_cols` order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueSource {
    /// A probe-side register, read at the current probe row.
    Probe(usize),
    /// A build-side payload column index, read at the matched build row --
    /// `Value::Null` for an unmatched (`Left`) probe row, matching
    /// `HashProbe`'s own NULL-fill for `payload_dst`.
    Payload(usize),
}

/// A running per-group accumulator for one `Opcode::HashProbeGroupReduce`
/// aggregate (#441) -- the fused-loop equivalent of collecting every
/// matched row's value into a `Vec<Value>` and calling [`reduce_values`]
/// once at the end (what `Opcode::GroupReduce` does): same output for
/// every [`AggFunc`], but O(1) per row instead of O(matched rows) memory,
/// which is the whole point of fusing the join and the reduce. Tracks
/// `non_null_count` (`COUNT(x)`'s denominator: every non-`Null` value)
/// separately from `numeric_count` (`SUM`/`AVG`/`MIN`/`MAX`'s: every value
/// [`Value::as_f64`] accepts) because a non-numeric, non-`Null` value
/// (e.g. a `Str`) counts for the former but not the latter -- exactly
/// [`reduce_values`]'s own distinction between its `values` and `non_null`
/// slices.
#[derive(Debug, Clone, Copy, Default)]
struct RunningAgg {
    row_count: i64,
    non_null_count: i64,
    numeric_count: i64,
    sum: f64,
    min: Option<f64>,
    max: Option<f64>,
}

impl RunningAgg {
    /// Folds one more row into this group's accumulator. `value` is
    /// `None` for `COUNT(*)` (no source column -- only `row_count`
    /// matters); `Some` for every other case, `Value::Null` included.
    fn push(&mut self, value: Option<&Value>) {
        self.row_count = self.row_count.saturating_add(1);
        let Some(value) = value else {
            return;
        };
        if !matches!(value, Value::Null) {
            self.non_null_count = self.non_null_count.saturating_add(1);
        }
        if let Some(x) = value.as_f64() {
            self.sum += x;
            self.numeric_count = self.numeric_count.saturating_add(1);
            self.min = Some(self.min.map_or(x, |m| m.min(x)));
            self.max = Some(self.max.map_or(x, |m| m.max(x)));
        }
    }

    /// The aggregate's final value, matching [`reduce_values`] exactly.
    /// `is_count_star` is `src.is_none()` on the originating `(AggFunc,
    /// Option<ValueSource>)` pair -- `COUNT(*)` counts every row
    /// regardless of `func`, `COUNT(x)` counts `x`'s non-`Null` rows.
    #[allow(
        clippy::cast_precision_loss,
        reason = "numeric_count only grows by +1 per row (`push`), so at any batch/segment-sized row count it converts to f64 without a precision-affecting magnitude"
    )]
    fn finalize(&self, func: AggFunc, is_count_star: bool) -> Value {
        match func {
            AggFunc::Count => Value::Int(if is_count_star {
                self.row_count
            } else {
                self.non_null_count
            }),
            AggFunc::Sum => {
                if self.numeric_count == 0 {
                    Value::Null
                } else {
                    Value::Float(self.sum)
                }
            }
            AggFunc::Avg => {
                if self.numeric_count == 0 {
                    Value::Null
                } else {
                    Value::Float(self.sum / self.numeric_count as f64)
                }
            }
            AggFunc::Min => self.min.map_or(Value::Null, Value::Float),
            AggFunc::Max => self.max.map_or(Value::Null, Value::Float),
        }
    }
}

/// Resolves one [`ValueSource`] for a given probe row / matched build row
/// (#441) -- `probe_columns` is a small (usually one-entry) list of
/// `(register, column)` pairs, linearly scanned rather than hashed since a
/// `GROUP BY`/aggregate over a join rarely references more than a
/// handful of distinct probe-side registers.
#[allow(
    clippy::indexing_slicing,
    reason = "`build_row`, when `Some`, always came from `BuildTable::index` in this same opcode's caller, so it is in range for `bt.payload`'s equal-length columns (same invariant `join_keys_match` relies on); `ValueSource::Probe` registers are looked up by value, not indexed"
)]
// A function returning `&Value` borrowed from two independent
// parameters (`probe_columns`, `bt`) needs an explicit lifetime tying
// them together, which is outside the qualified subset (function-scoped
// elision only) -- so this hands the resolved value to a callback
// instead, staying inside one function's scope and never naming a
// lifetime at all.
fn with_value_source<R>(
    source: ValueSource,
    probe_columns: &[(usize, &[Value])],
    probe_row: usize,
    bt: &BuildTable,
    build_row: Option<usize>,
    f: impl FnOnce(&Value) -> R,
) -> R {
    match source {
        ValueSource::Probe(reg) => match probe_columns.iter().find(|(r, _)| *r == reg) {
            Some((_, col)) => f(&col[probe_row]),
            None => f(&Value::Null),
        },
        ValueSource::Payload(i) => match build_row {
            Some(br) => f(&bt.payload[i][br]),
            None => f(&Value::Null),
        },
    }
}

/// Folds one joined (or, for an unmatched `Left` probe row, NULL-payload)
/// row into `Opcode::HashProbeGroupReduce`'s running group state (#441) --
/// the fused equivalent of `Opcode::GroupReduce`'s per-row grouping loop,
/// except values are read straight from `probe_columns`/`bt` instead of
/// from a materialized joined row, and folded into a [`RunningAgg`]
/// instead of collected into a per-group `Vec<Value>`.
#[allow(
    clippy::too_many_arguments,
    reason = "this is the fused hot loop's entire per-row state; bundling it into an ad-hoc struct used nowhere else would not simplify anything"
)]
#[allow(
    clippy::indexing_slicing,
    reason = "`g` (an existing group) always came from `group_keys`/`accumulators`, which grow together one entry per new group (below); `group_keys[g]`/`accumulators[i][g]` are therefore always in range"
)]
fn fold_group_row(
    group_by: &[(ValueSource, usize)],
    aggs: &[(AggFunc, Option<ValueSource>)],
    probe_columns: &[(usize, &[Value])],
    bt: &BuildTable,
    probe_row: usize,
    build_row: Option<usize>,
    group_index: &mut HashMap<u64, Vec<usize>>,
    group_keys: &mut Vec<Vec<Value>>,
    accumulators: &mut [Vec<RunningAgg>],
) {
    // #439-style probe-before-insert: hash and compare each `group_by`
    // component as it's resolved, and only collect a `Vec<Value>` key
    // (one allocation) on a genuine new group -- an existing group (the
    // overwhelming majority of rows once there are only a handful of
    // groups) allocates nothing here.
    let mut hasher = DefaultHasher::new();
    for (src, _) in group_by {
        with_value_source(*src, probe_columns, probe_row, bt, build_row, |value| {
            hash_group_value(value, &mut hasher);
        });
    }
    let hash = hasher.finish();
    let bucket = group_index.entry(hash).or_default();
    let existing = bucket.iter().copied().find(|&g| {
        group_by.iter().enumerate().all(|(i, (src, _))| {
            with_value_source(*src, probe_columns, probe_row, bt, build_row, |value| {
                group_keys[g][i] == *value
            })
        })
    });
    let group = match existing {
        Some(g) => g,
        None => {
            let key_values: Vec<Value> = group_by
                .iter()
                .map(|(src, _)| {
                    with_value_source(*src, probe_columns, probe_row, bt, build_row, Value::clone)
                })
                .collect();
            let g = group_keys.len();
            group_keys.push(key_values);
            bucket.push(g);
            for accs in accumulators.iter_mut() {
                accs.push(RunningAgg::default());
            }
            g
        }
    };
    for (i, (_func, src)) in aggs.iter().enumerate() {
        match src {
            Some(s) => with_value_source(*s, probe_columns, probe_row, bt, build_row, |value| {
                accumulators[i][group].push(Some(value));
            }),
            None => accumulators[i][group].push(None),
        }
    }
}

/// The join hash tables a [`Vm`] has built (`Opcode::HashBuild`), keyed
/// by table id -- an opaque, cheaply clonable handle (#272). Build once,
/// then hand a clone to every probe-side [`Vm`] via
/// [`Vm::with_join_tables`] so parallel workers share one table instead
/// of each rebuilding (or cloning) it. Cloning is an `Arc` bump per
/// table, never a copy of the entries.
#[derive(Debug, Clone, Default)]
pub struct JoinTables(HashMap<usize, Arc<BuildTable>>);

/// A pending, not-yet-applied [`Opcode::Filter`] result (#265): `indices`
/// are the surviving row positions in the space every live register
/// still occupies -- `Filter` no longer eagerly compacts every register,
/// so `base_len` is the length they are still expected to have. Resolved
/// lazily by whichever opcode actually needs dense rows ([`Opcode::Emit`],
/// [`Opcode::GroupReduce`], [`Opcode::HashBuild`], indexing through
/// `indices` only for the registers it reads); every other
/// register-reading opcode forces an eager compaction first via
/// [`Vm::resolve_selection`], reproducing pre-#265 behavior exactly from
/// that point on.
#[derive(Debug)]
struct Selection {
    base_len: usize,
    indices: Vec<u32>,
}

/// A register machine executing one batch at a time.
#[derive(Default)]
pub struct Vm {
    registers: HashMap<usize, Arc<Vec<Value>>>,
    /// Typed registers (#429): a register loaded from a
    /// [`Batch::typed_columns`] entry lives here instead of `registers`,
    /// with no per-row `Value` materialized. Opcodes ported to `Column`
    /// dispatch (currently `Filter`'s Bool predicate, and `Map`'s
    /// `Eq`/`Ne` against a `Dict` column) read it directly; every other
    /// opcode reads through [`Vm::reg`], which materializes on demand.
    typed_registers: HashMap<usize, Arc<Column>>,
    output: QueryOutput,
    join_tables: JoinTables,
    /// A pending `Filter` result not yet applied to `registers`. See
    /// [`Selection`].
    selection: Option<Selection>,
    /// Instructions executed so far, checked against [`MAX_STEPS`] by
    /// [`Vm::execute`]/[`Vm::run`].
    steps: usize,
}

impl fmt::Debug for Vm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vm")
            .field("registers", &self.registers)
            .field("output", &self.output)
            .field(
                "join_tables",
                &self.join_tables.0.keys().collect::<Vec<_>>(),
            )
            .field("selection", &self.selection)
            .finish()
    }
}

impl Vm {
    /// A fresh VM with no registers, no join tables and no output.
    pub fn new() -> Self {
        Vm::default()
    }

    /// A fresh VM that already holds `tables` -- the probe side of a join
    /// whose build side ran in another [`Vm`] (#272). Sharing is by `Arc`,
    /// so many probe VMs (one per segment, on as many threads) read one
    /// built table.
    pub fn with_join_tables(tables: JoinTables) -> Self {
        Vm {
            join_tables: tables,
            ..Vm::default()
        }
    }

    /// The join tables built so far, as a shareable handle; the VM keeps
    /// its own reference too. See [`Vm::with_join_tables`].
    pub fn join_tables(&self) -> JoinTables {
        self.join_tables.clone()
    }

    /// The current contents of register `reg`, or
    /// [`VmError::UnknownRegister`] if it has never been written.
    pub fn register(&self, reg: usize) -> Result<&[Value]> {
        self.reg(reg, "register")
    }

    /// The current contents of typed register `reg` (#429) -- populated
    /// alongside `register`'s `Vec<Value>` copy by
    /// [`Opcode::LoadColumn`], for opcodes that dispatch on [`Column`]
    /// directly instead of paying the per-element `Value` cost. `None`
    /// if `reg` was never loaded from a [`Batch::typed_columns`] entry.
    pub fn typed_register(&self, reg: usize) -> Option<&Column> {
        self.typed_registers.get(&reg).map(Arc::as_ref)
    }

    /// Move register `reg` out of the VM (it becomes unknown afterwards),
    /// or [`VmError::UnknownRegister`] if it was never written -- for
    /// callers assembling a [`Batch`] from finished registers without
    /// copying every cell (#272). Since #264 a register is `Arc`-shared,
    /// so this only actually avoids the copy when the VM is this
    /// register's sole owner (`Arc::try_unwrap` succeeds); a register
    /// still shared with its batch (e.g. an untransformed `LoadColumn`)
    /// is cloned instead.
    pub fn take_register(&mut self, reg: usize) -> Result<Vec<Value>> {
        self.typed_registers.remove(&reg);
        let values = self
            .registers
            .remove(&reg)
            .ok_or(VmError::UnknownRegister {
                opcode: "take_register",
                register: reg,
            })?;
        Ok(Arc::try_unwrap(values).unwrap_or_else(|shared| (*shared).clone()))
    }

    /// Like [`Self::register`], but tags an unknown-register error with the
    /// opcode that requested it, for callers inside [`Self::step`].
    fn reg(&self, reg: usize, opcode: &'static str) -> Result<&[Value]> {
        self.registers
            .get(&reg)
            .map(|values| values.as_slice())
            .ok_or(VmError::UnknownRegister {
                opcode,
                register: reg,
            })
    }

    /// Count one more executed instruction, failing once [`MAX_STEPS`] is
    /// exceeded so a pathological or buggy compiled program can't run
    /// forever.
    fn check_step_limit(&mut self, opcode: &'static str) -> Result<()> {
        self.steps = self.steps.saturating_add(1);
        if self.steps > MAX_STEPS {
            return Err(VmError::StepLimitExceeded {
                opcode,
                limit: MAX_STEPS,
            });
        }
        Ok(())
    }

    /// Runs every opcode of `program` against `batch` in order, stopping at
    /// the first error. Unlike [`Vm::run`], this executes a single batch and
    /// does no segment looping.
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
    /// build-side registers with the wrong length. Also drops any pending
    /// selection (#265): a build-side `Vec<u32>` of row indices is just as
    /// meaningless against the next program's batch as a leftover register.
    pub fn clear_registers(&mut self) {
        self.registers.clear();
        self.selection = None;
    }

    /// Eagerly compacts every live register down to the pending
    /// [`Selection`] (if any) -- `Filter`'s pre-#265 behavior, now invoked
    /// lazily by opcodes that read registers elementwise rather than by
    /// row index (`Map`, `Reduce`, `Window`, and `HashProbe`'s key
    /// columns) instead of unconditionally by `Filter` itself. A no-op
    /// once nothing is pending.
    fn resolve_selection(&mut self, opcode: &'static str) -> Result<()> {
        let Some(selection) = self.selection.take() else {
            return Ok(());
        };
        for values in self.registers.values_mut() {
            if values.len() != selection.base_len {
                return Err(VmError::RegisterLengthMismatch { opcode });
            }
            let mut compacted = Vec::with_capacity(selection.indices.len());
            for &idx in &selection.indices {
                let value = values.get(idx as usize).ok_or(VmError::MalformedProgram {
                    opcode,
                    reason: "selection index out of range".to_string(),
                })?;
                compacted.push(value.clone());
            }
            *values = Arc::new(compacted);
        }
        // #429: `typed_registers` shadows a `registers` entry loaded
        // straight from `Batch::typed_columns`, uncompacted. Once a
        // selection is resolved, `registers` above just got compacted to
        // the surviving rows while any typed shadow would still be the
        // original full-length `Column` -- stale and wrong-length for
        // `Filter`/`Map`'s fast paths to read afterward. Clearing them
        // is conservative (falls back to the general `Value` path for
        // that register from here on) but always correct.
        self.typed_registers.clear();
        Ok(())
    }

    /// Fast path for [`Opcode::Map`] with `Eq`/`Ne` where one operand is a
    /// [`Column::Dict`] register and the other resolves to a single
    /// string literal (a `LoadConst`-broadcast register, or any register
    /// whose every value is the same `Value::Str`): compares dictionary
    /// codes instead of decoding every row's string (db-core#399/#429).
    /// `None` when neither operand fits that shape, so the caller falls
    /// back to the general elementwise [`apply_map_op`] path.
    fn dict_literal_compare(
        &self,
        op: MapOp,
        a: usize,
        b: usize,
        opcode: &'static str,
    ) -> Result<Option<Vec<Value>>> {
        if !matches!(op, MapOp::Eq | MapOp::Ne) {
            return Ok(None);
        }
        for (dict_reg, other_reg) in [(a, b), (b, a)] {
            let Some(column) = self.typed_registers.get(&dict_reg) else {
                continue;
            };
            let Column::Dict {
                dict,
                indices,
                valid,
            } = column.as_ref()
            else {
                continue;
            };
            let other = self.reg(other_reg, opcode)?;
            let Some(literal) = single_str_literal(other) else {
                continue;
            };
            let code = dict.iter().position(|s| s.as_ref() == literal);
            let result = (0..indices.len())
                .map(|i| {
                    if !valid.get(i) {
                        return Value::Null;
                    }
                    let idx_code = indices.get(i).and_then(|&idx| usize::try_from(idx).ok());
                    let is_eq = idx_code == code;
                    Value::Bool(if op == MapOp::Eq { is_eq } else { !is_eq })
                })
                .collect();
            return Ok(Some(result));
        }
        Ok(None)
    }

    /// Fast path for [`Opcode::Map`]'s `Add`/`Sub`/`Mul`/`Div` when
    /// *both* operands are typed `Int`/`Float` columns (#130 child 4
    /// slice 2, db-core#431): computes over the packed buffers, honoring
    /// the same NULL-propagation and Int/Float promotion rules as
    /// [`arithmetic`] (Int stays Int unless `Div`; either NULL operand
    /// yields a NULL row). `None` when the op isn't one of these four or
    /// either operand isn't a typed numeric column (including a
    /// `LoadConst`-broadcast literal, which is a plain `Vec<Value>`
    /// register, not a typed one) -- the caller falls back to the
    /// general elementwise path, which handles that case correctly, just
    /// without the typed fast path.
    fn typed_arithmetic(
        &self,
        op: MapOp,
        a: usize,
        b: usize,
        opcode: &'static str,
    ) -> Result<Option<Column>> {
        if !matches!(op, MapOp::Add | MapOp::Sub | MapOp::Mul | MapOp::Div) {
            return Ok(None);
        }
        let (Some(a_col), Some(b_col)) =
            (self.typed_registers.get(&a), self.typed_registers.get(&b))
        else {
            return Ok(None);
        };
        if !matches!(a_col.as_ref(), Column::Int { .. } | Column::Float { .. })
            || !matches!(b_col.as_ref(), Column::Int { .. } | Column::Float { .. })
        {
            return Ok(None);
        }
        if a_col.len() != b_col.len() {
            return Err(VmError::RegisterLengthMismatch { opcode });
        }
        let len = a_col.len();
        let result_is_int = matches!(a_col.as_ref(), Column::Int { .. })
            && matches!(b_col.as_ref(), Column::Int { .. })
            && op != MapOp::Div;
        let mut valid_bits = Vec::with_capacity(len);
        if result_is_int {
            let mut data = Vec::with_capacity(len);
            for i in 0..len {
                match (column_num_at(a_col, i), column_num_at(b_col, i)) {
                    (Some((x, _)), Some((y, _))) => {
                        #[allow(
                            clippy::cast_possible_truncation,
                            reason = "saturating f64 -> i64 is arithmetic's documented overflow semantics, mirrored here for the typed fast path"
                        )]
                        data.push(arith_op(op, x, y) as i64);
                        valid_bits.push(true);
                    }
                    _ => {
                        data.push(0);
                        valid_bits.push(false);
                    }
                }
            }
            Ok(Some(Column::Int {
                data,
                valid: Bitmap::from_bools(valid_bits.into_iter()),
            }))
        } else {
            let mut data = Vec::with_capacity(len);
            for i in 0..len {
                match (column_num_at(a_col, i), column_num_at(b_col, i)) {
                    (Some((x, _)), Some((y, _))) => {
                        data.push(arith_op(op, x, y));
                        valid_bits.push(true);
                    }
                    _ => {
                        data.push(0.0);
                        valid_bits.push(false);
                    }
                }
            }
            Ok(Some(Column::Float {
                data,
                valid: Bitmap::from_bools(valid_bits.into_iter()),
            }))
        }
    }

    /// Take (and clear) the rows collected so far by [`Opcode::Emit`] --
    /// for callers driving [`Self::execute`] batch-by-batch themselves
    /// (e.g. a bounded scan that stops once enough rows are collected,
    /// #108) rather than via [`Self::run`]/[`run_parallel`].
    pub fn take_output(&mut self) -> QueryOutput {
        std::mem::take(&mut self.output)
    }

    /// Drive `program` across every batch `source` yields, honoring
    /// [`Opcode::Scan`]/[`Opcode::NextSegment`]/[`Opcode::Halt`] control
    /// flow, and return the rows collected by [`Opcode::Emit`].
    pub fn run<T: Source>(&mut self, source: &mut T, program: &[Opcode]) -> Result<QueryOutput> {
        self.output = QueryOutput::default();
        let mut batch = match source.next_batch() {
            Some(b) => b,
            None => return Ok(QueryOutput::default()),
        };
        self.selection = None;
        let mut pc = 0usize;
        while let Some(op) = program.get(pc) {
            self.check_step_limit(op.name())?;
            match op {
                Opcode::NextSegment { loop_start } => match source.next_batch() {
                    Some(next) => {
                        batch = next;
                        // #265: a new segment's registers start over
                        // (`LoadColumn` re-loads full columns at the top
                        // of the loop body), so any selection pending
                        // from the previous segment is meaningless here.
                        self.selection = None;
                        pc = *loop_start;
                    }
                    None => pc = pc.saturating_add(1),
                },
                Opcode::Halt => break,
                other => {
                    self.step(&batch, other)?;
                    pc = pc.saturating_add(1);
                }
            }
        }
        Ok(std::mem::take(&mut self.output))
    }

    #[allow(
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::needless_range_loop,
        reason = "every register and batch column holds exactly `num_rows` values (checked via `RegisterLengthMismatch` where two are combined), so `row`/`group` drawn from `0..num_rows` and group ids from `group_keys` are in range; the `len() - 1` follows a push. `0..num_rows` loops that index a same-length `hashes: Vec<u64>` (#439/#440) also call `physical(row)` or otherwise use `row` beyond that one index, so `enumerate()` would not simplify them"
    )]
    fn step(&mut self, batch: &Batch, op: &Opcode) -> Result<()> {
        let opcode = op.name();
        match op {
            Opcode::LoadColumn { reg, column } => {
                if let Some(typed) = batch.typed_columns.get(column.as_ref()) {
                    // #429: also materialize into `registers` so every
                    // opcode not yet ported to `Column` dispatch (#130
                    // child 4) keeps working completely unchanged --
                    // `typed_registers` is the accelerated path `Filter`
                    // and `Map`'s `Dict`-literal comparison consult
                    // instead of paying this cost.
                    let materialized: Vec<Value> = (0..typed.len()).map(|i| typed.get(i)).collect();
                    self.registers.insert(*reg, Arc::new(materialized));
                    self.typed_registers.insert(*reg, Arc::clone(typed));
                } else {
                    let values = batch.columns.get(column.as_ref()).ok_or_else(|| {
                        VmError::UnknownColumn {
                            opcode,
                            column: column.to_string(),
                        }
                    })?;
                    self.registers.insert(*reg, Arc::clone(values));
                    self.typed_registers.remove(reg);
                }
            }
            Opcode::LoadConst { reg, value } => {
                self.registers
                    .insert(*reg, Arc::new(vec![value.clone(); batch.num_rows]));
            }
            Opcode::Map { dst, op, a, b } => {
                // #265: reads registers elementwise (no row-index
                // concept), so any pending selection must be resolved
                // (compacted) first rather than taught to this opcode.
                self.resolve_selection(opcode)?;
                if let Some(result) = self.dict_literal_compare(*op, *a, *b, opcode)? {
                    self.registers.insert(*dst, Arc::new(result));
                    self.typed_registers.remove(dst);
                } else if let Some(column) = self.typed_arithmetic(*op, *a, *b, opcode)? {
                    // #431: a genuine typed Column result -- also
                    // materialize into `registers` (same pattern as
                    // `LoadColumn`, #429) so a downstream un-migrated
                    // opcode keeps working unchanged.
                    let materialized: Vec<Value> =
                        (0..column.len()).map(|i| column.get(i)).collect();
                    self.registers.insert(*dst, Arc::new(materialized));
                    self.typed_registers.insert(*dst, Arc::new(column));
                } else {
                    let (a_vals, b_vals) = (self.reg(*a, opcode)?, self.reg(*b, opcode)?);
                    if a_vals.len() != b_vals.len() {
                        return Err(VmError::RegisterLengthMismatch { opcode });
                    }
                    let result: Vec<Value> = a_vals
                        .iter()
                        .zip(b_vals.iter())
                        .map(|(x, y)| apply_map_op(*op, x, y))
                        .collect();
                    self.registers.insert(*dst, Arc::new(result));
                    self.typed_registers.remove(dst);
                }
            }
            Opcode::Call { dst, name, args } => {
                // Elementwise, like `Map`: resolve any pending selection
                // first (see the comment on that arm).
                self.resolve_selection(opcode)?;
                let arg_regs: Vec<&[Value]> = args
                    .iter()
                    .map(|&a| self.reg(a, opcode))
                    .collect::<Result<_>>()?;
                let num_rows = arg_regs.first().map_or(batch.num_rows, |r| r.len());
                if arg_regs.iter().any(|r| r.len() != num_rows) {
                    return Err(VmError::RegisterLengthMismatch { opcode });
                }
                let mut call_args = vec![crate::value::Value::Null; arg_regs.len()];
                let mut result = Vec::with_capacity(num_rows);
                for row in 0..num_rows {
                    for (slot, reg) in call_args.iter_mut().zip(&arg_regs) {
                        *slot = to_scalar_value(&reg[row]);
                    }
                    let value = crate::functions::call(name, &call_args)
                        .map_or(Value::Null, from_scalar_value);
                    result.push(value);
                }
                self.registers.insert(*dst, Arc::new(result));
            }
            Opcode::Filter { predicate } => {
                // #265: produces a selection vector (surviving row
                // indices) instead of eagerly compacting every live
                // register -- registers stay exactly as they are;
                // whichever opcode later actually needs dense rows
                // resolves this (see `Selection`).
                let mask: Vec<bool> = match self.typed_registers.get(predicate) {
                    // #429: a typed Bool predicate register (e.g. loaded
                    // straight from a sealed segment's Bool column) reads
                    // its bitmap-backed buffer directly, skipping the
                    // `Value::Bool(true)` match entirely.
                    Some(column) => match column.as_ref() {
                        Column::Bool { data, valid } => (0..data.len())
                            .map(|i| valid.get(i) && data.get(i).copied().unwrap_or(false))
                            .collect(),
                        other => (0..other.len())
                            .map(|i| matches!(other.get(i), Value::Bool(true)))
                            .collect(),
                    },
                    None => self
                        .reg(*predicate, opcode)?
                        .iter()
                        .map(|v| matches!(v, Value::Bool(true)))
                        .collect(),
                };
                let base_len = mask.len();
                for values in self.registers.values() {
                    if values.len() != base_len {
                        return Err(VmError::RegisterLengthMismatch { opcode });
                    }
                }
                let source_indices: Vec<u32> = match self.selection.take() {
                    Some(existing) => {
                        if existing.base_len != base_len {
                            return Err(VmError::RegisterLengthMismatch { opcode });
                        }
                        existing.indices
                    }
                    // #110: size the surviving-index buffer to the actual
                    // survivor count up front (not the pre-filter length)
                    // -- at 50% selectivity on a 10M-row scan, over-
                    // allocating by 2x is real peak-RSS waste for no
                    // benefit (the excess capacity is never used).
                    None => (0..u32::try_from(base_len).unwrap_or(u32::MAX)).collect(),
                };
                let kept_len = mask.iter().filter(|&&keep| keep).count();
                let mut indices = Vec::with_capacity(kept_len);
                for idx in source_indices {
                    if mask.get(idx as usize).copied().unwrap_or(false) {
                        indices.push(idx);
                    }
                }
                self.selection = Some(Selection { base_len, indices });
            }
            Opcode::Reduce { func, src, dst } => {
                // #265: see the comment on `Map`'s same call. Also
                // clears `typed_registers` when it actually resolves a
                // selection, so the typed fast path below only ever
                // fires over an unfiltered (or already-compacted) typed
                // column -- never a stale, wrong-length shadow.
                self.resolve_selection(opcode)?;
                let result = match src {
                    Some(reg) => {
                        let typed = self
                            .typed_registers
                            .get(reg)
                            .and_then(|column| typed_reduce_values(*func, column));
                        match typed {
                            Some(result) => result,
                            None => reduce_values(*func, self.reg(*reg, opcode)?),
                        }
                    }
                    None => reduce_count_star(*func, batch.num_rows),
                };
                self.registers.insert(*dst, Arc::new(vec![result]));
                self.typed_registers.remove(dst);
            }
            Opcode::GroupReduce {
                group_by,
                aggs,
                agg_dst,
            } => {
                // #265: lazily resolves any pending selection -- only the
                // group-key and per-aggregate source registers this
                // opcode actually reads are indexed through it, instead
                // of every live register being eagerly compacted back in
                // `Filter`. Taken up front so the borrows below are of
                // `self.registers` alone, not all of `self`.
                let selection = self.selection.take();
                let key_columns: Vec<&[Value]> = group_by
                    .iter()
                    .map(|reg| self.reg(*reg, opcode))
                    .collect::<Result<_>>()?;
                let base_len = match &selection {
                    Some(sel) => sel.base_len,
                    None => match key_columns.first() {
                        Some(c) => c.len(),
                        None => match aggs.iter().find_map(|(_, src)| {
                            src.map(|reg| self.reg(reg, opcode).map(<[Value]>::len))
                        }) {
                            Some(len) => len?,
                            // `COUNT(*)` alone: no key or source column
                            // carries the row count, but every live
                            // register has the post-`Filter` length
                            // (RegisterLengthMismatch guards that), so any
                            // one of them is it; with no register at all
                            // nothing was filtered and the batch's own row
                            // count is exact.
                            None => self
                                .registers
                                .values()
                                .next()
                                .map_or(batch.num_rows, |v| v.len()),
                        },
                    },
                };
                for c in &key_columns {
                    if c.len() != base_len {
                        return Err(VmError::RegisterLengthMismatch { opcode });
                    }
                }
                let num_rows = selection.as_ref().map_or(base_len, |sel| sel.indices.len());
                let physical = |row: usize| {
                    selection
                        .as_ref()
                        .map_or(row, |sel| sel.indices[row] as usize)
                };

                // #439/#440: hash column-wise (`hash_columns_by_row`, no
                // per-row key materialized), then probe by hash first on a
                // borrowed row view, only clone+allocate a `Vec<Value>`
                // key on a genuine new group -- the old code built and
                // cloned that `Vec` (plus its `String`s) for every input
                // row, even though nearly every row lands in a group that
                // already exists. `group_index` buckets group ids by hash
                // (collisions possible, so each bucket is checked for an
                // exact match) rather than owning a `GroupKey` per entry.
                let hashes = hash_columns_by_row(&key_columns, num_rows, physical);
                let mut group_index: HashMap<u64, Vec<usize>> = HashMap::new();
                let mut group_keys: Vec<Vec<Value>> = Vec::new();
                let mut row_group: Vec<usize> = Vec::with_capacity(num_rows);
                for row in 0..num_rows {
                    let p = physical(row);
                    let bucket = group_index.entry(hashes[row]).or_default();
                    let existing = bucket.iter().copied().find(|&g| {
                        key_columns
                            .iter()
                            .enumerate()
                            .all(|(i, c)| c[p] == group_keys[g][i])
                    });
                    let group = match existing {
                        Some(g) => g,
                        None => {
                            let key: Vec<Value> =
                                key_columns.iter().map(|c| c[p].clone()).collect();
                            let g = group_keys.len();
                            group_keys.push(key);
                            bucket.push(g);
                            g
                        }
                    };
                    row_group.push(group);
                }
                let num_groups = group_keys.len();

                for (i, reg) in group_by.iter().enumerate() {
                    let column: Vec<Value> = group_keys.iter().map(|k| k[i].clone()).collect();
                    self.registers.insert(*reg, Arc::new(column));
                }

                for ((func, src), dst) in aggs.iter().zip(agg_dst.iter()) {
                    let mut per_group: Vec<Vec<Value>> = vec![Vec::new(); num_groups];
                    match src {
                        Some(reg) => {
                            // #110: borrow instead of `.to_vec()` -- same
                            // redundant-clone pattern as the old `Emit`.
                            let values = self.reg(*reg, opcode)?;
                            if values.len() != base_len {
                                return Err(VmError::RegisterLengthMismatch { opcode });
                            }
                            for (row, group) in row_group.iter().enumerate() {
                                per_group[*group].push(values[physical(row)].clone());
                            }
                        }
                        None => {
                            for group in &row_group {
                                per_group[*group].push(Value::Null);
                            }
                        }
                    }
                    let result: Vec<Value> = per_group
                        .iter()
                        .map(|vals| {
                            if src.is_none() {
                                Value::Int(len_to_i64(vals.len()))
                            } else {
                                reduce_values(*func, vals)
                            }
                        })
                        .collect();
                    self.registers.insert(*dst, Arc::new(result));
                }
            }
            Opcode::HashBuild {
                key_cols,
                payload_cols,
                table,
            } => {
                // #265: lazily resolves any pending selection -- only the
                // key/payload registers this opcode actually reads are
                // indexed through it, rather than every live register
                // being eagerly compacted back in `Filter`. Taken up
                // front so the borrows below are of `self.registers`
                // alone, not all of `self`.
                let selection = self.selection.take();
                let key_columns: Vec<&[Value]> = key_cols
                    .iter()
                    .map(|r| self.reg(*r, opcode))
                    .collect::<Result<_>>()?;
                let payload_columns: Vec<&[Value]> = payload_cols
                    .iter()
                    .map(|r| self.reg(*r, opcode))
                    .collect::<Result<_>>()?;
                let base_len = key_columns.first().map(|c| c.len()).ok_or_else(|| {
                    VmError::MalformedProgram {
                        opcode,
                        reason: "hash build has no key columns".to_string(),
                    }
                })?;
                for c in key_columns.iter().chain(&payload_columns) {
                    if c.len() != base_len {
                        return Err(VmError::RegisterLengthMismatch { opcode });
                    }
                }
                if let Some(sel) = &selection {
                    if sel.base_len != base_len {
                        return Err(VmError::RegisterLengthMismatch { opcode });
                    }
                }
                let num_rows = selection.as_ref().map_or(base_len, |sel| sel.indices.len());
                let physical = |row: usize| {
                    selection
                        .as_ref()
                        .map_or(row, |sel| sel.indices[row] as usize)
                };

                // #440: hash the key columns column-wise (no per-row
                // `Vec<Value>` key), then store key/payload data
                // column-major too -- one allocation per key/payload
                // column (reserved up front), not one per row. `index`
                // maps each row's hash to its own row number; a later
                // probe still has to verify a candidate row's actual
                // values (`join_keys_match`), since two different keys
                // can share a hash.
                let hashes = hash_columns_by_row(&key_columns, num_rows, physical);
                let mut keys: Vec<Vec<Value>> = key_columns
                    .iter()
                    .map(|_| Vec::with_capacity(num_rows))
                    .collect();
                let mut payload: Vec<Vec<Value>> = payload_columns
                    .iter()
                    .map(|_| Vec::with_capacity(num_rows))
                    .collect();
                let mut index: JoinHashTable<u64, usize> = JoinHashTable::with_capacity(num_rows);
                for row in 0..num_rows {
                    let p = physical(row);
                    for (i, c) in key_columns.iter().enumerate() {
                        keys[i].push(c[p].clone());
                    }
                    for (i, c) in payload_columns.iter().enumerate() {
                        payload[i].push(c[p].clone());
                    }
                    index.insert(hashes[row], row);
                }
                self.join_tables.0.insert(
                    *table,
                    Arc::new(BuildTable {
                        index,
                        keys,
                        payload,
                    }),
                );
            }
            Opcode::HashProbe {
                key_cols,
                table,
                payload_dst,
                kind,
            } => {
                // #265: HashProbe already reshapes every live register
                // itself (below) based on its own match/fan-out indices,
                // a different and richer concept than a plain `Filter`
                // selection vector -- resolve any pending selection first
                // so the rest of this arm is unchanged from before #265.
                self.resolve_selection(opcode)?;
                let key_columns: Vec<&[Value]> = key_cols
                    .iter()
                    .map(|r| self.reg(*r, opcode))
                    .collect::<Result<_>>()?;
                let num_rows = key_columns.first().map(|c| c.len()).ok_or_else(|| {
                    VmError::MalformedProgram {
                        opcode,
                        reason: "hash probe has no key columns".to_string(),
                    }
                })?;
                // An `Arc` bump, so `bt` is a local the payload columns
                // below can read from after the reshape has mutably
                // borrowed `self.registers` (#272: no payload clone per
                // matched row -- `emitted` records the build row, and
                // each payload cell is cloned once, straight into its
                // destination column).
                let bt = Arc::clone(self.join_tables.0.get(table).ok_or(
                    VmError::UnknownJoinTable {
                        opcode,
                        table: *table,
                    },
                )?);

                // #440: hash the probe rows column-wise (no per-row
                // `Vec<Value>` key, not even a reused buffer) and probe
                // `bt.index` by hash; `join_keys_match` resolves both
                // genuine hash collisions and NULL-never-matches against
                // `bt.keys` before a candidate counts as a real match.
                let hashes = hash_columns_by_row(&key_columns, num_rows, |row| row);
                let mut emitted: Vec<(usize, Option<usize>)> = Vec::with_capacity(num_rows);
                for row in 0..num_rows {
                    let mut matched = false;
                    let emit_payload =
                        !matches!(kind, JoinKind::Semi) && should_emit(*kind, true, true);
                    bt.index.for_each_match_slot(&hashes[row], |slot| {
                        let Some(&build_row) = bt.index.value_at(slot) else {
                            return;
                        };
                        if !join_keys_match(&key_columns, row, &bt.keys, build_row) {
                            return;
                        }
                        matched = true;
                        if emit_payload {
                            emitted.push((row, Some(build_row)));
                        }
                    });
                    if !matched {
                        if should_emit(*kind, false, false) {
                            emitted.push((row, None));
                        }
                    } else if matches!(kind, JoinKind::Semi) {
                        // At most one row per probe-side match, regardless
                        // of build-side fanout; no payload (see doc comment
                        // on `Opcode::HashProbe`).
                        emitted.push((row, None));
                    }
                }

                for values in self.registers.values_mut() {
                    if values.len() != num_rows {
                        return Err(VmError::RegisterLengthMismatch { opcode });
                    }
                    let reshaped: Vec<Value> = emitted
                        .iter()
                        .map(|(row, _)| values[*row].clone())
                        .collect();
                    *values = Arc::new(reshaped);
                }

                for (i, dst) in payload_dst.iter().enumerate() {
                    let col: Vec<Value> = emitted
                        .iter()
                        .map(|(_, build_row)| match build_row {
                            // `None` is an unmatched LEFT JOIN probe row: NULL
                            // by definition. A payload narrower than its
                            // destinations is a planner bug, not NULL data;
                            // `build_row` always indexes a real row of
                            // `bt.payload` (it came from `bt.index` itself),
                            // so only the column count can be wrong.
                            Some(build_row) => bt
                                .payload
                                .get(i)
                                .and_then(|column| column.get(*build_row))
                                .cloned()
                                .ok_or_else(|| VmError::MalformedProgram {
                                    opcode,
                                    reason: format!(
                                        "join payload has {} columns but destination {i} was requested",
                                        bt.payload.len()
                                    ),
                                }),
                            None => Ok(Value::Null),
                        })
                        .collect::<Result<_>>()?;
                    self.registers.insert(*dst, Arc::new(col));
                }
            }
            Opcode::HashProbeGroupReduce {
                key_cols,
                table,
                kind,
                group_by,
                aggs,
                agg_dst,
            } => {
                // #441: same probe-resolution rule as `Opcode::HashProbe`
                // above -- this opcode reshapes nothing (there is no
                // per-row output to reshape), but still reads live
                // registers by row index below, so any pending selection
                // must be resolved first.
                self.resolve_selection(opcode)?;
                let key_columns: Vec<&[Value]> = key_cols
                    .iter()
                    .map(|r| self.reg(*r, opcode))
                    .collect::<Result<_>>()?;
                let num_rows = key_columns.first().map(|c| c.len()).ok_or_else(|| {
                    VmError::MalformedProgram {
                        opcode,
                        reason: "hash probe has no key columns".to_string(),
                    }
                })?;
                let bt = Arc::clone(self.join_tables.0.get(table).ok_or(
                    VmError::UnknownJoinTable {
                        opcode,
                        table: *table,
                    },
                )?);

                // Every distinct probe-side register a `group_by`/`aggs`
                // source reads -- resolved once up front, like
                // `key_columns`, rather than re-looked-up per row.
                let mut probe_columns: Vec<(usize, &[Value])> = Vec::new();
                for reg in group_by
                    .iter()
                    .filter_map(|(src, _)| match src {
                        ValueSource::Probe(reg) => Some(*reg),
                        ValueSource::Payload(_) => None,
                    })
                    .chain(aggs.iter().filter_map(|(_, src)| match src {
                        Some(ValueSource::Probe(reg)) => Some(*reg),
                        _ => None,
                    }))
                {
                    if !probe_columns.iter().any(|(r, _)| *r == reg) {
                        probe_columns.push((reg, self.reg(reg, opcode)?));
                    }
                }

                // #440-style hash-then-verify probe, but folding each real
                // match straight into a group's `RunningAgg`s instead of
                // recording `(row, build_row)` pairs to reshape later
                // (#439's `HashProbe`) or materializing a joined row at
                // all (this is the whole point of #441): a join feeding
                // only an aggregate never allocates one register set per
                // matched row, only one `RunningAgg` per *group*.
                let hashes = hash_columns_by_row(&key_columns, num_rows, |row| row);
                let mut group_index: HashMap<u64, Vec<usize>> = HashMap::new();
                let mut group_keys: Vec<Vec<Value>> = Vec::new();
                let mut accumulators: Vec<Vec<RunningAgg>> = vec![Vec::new(); aggs.len()];
                for row in 0..num_rows {
                    let mut matched = false;
                    let emit_payload =
                        !matches!(kind, JoinKind::Semi) && should_emit(*kind, true, true);
                    bt.index.for_each_match_slot(&hashes[row], |slot| {
                        let Some(&build_row) = bt.index.value_at(slot) else {
                            return;
                        };
                        if !join_keys_match(&key_columns, row, &bt.keys, build_row) {
                            return;
                        }
                        matched = true;
                        if emit_payload {
                            fold_group_row(
                                group_by,
                                aggs,
                                &probe_columns,
                                &bt,
                                row,
                                Some(build_row),
                                &mut group_index,
                                &mut group_keys,
                                &mut accumulators,
                            );
                        }
                    });
                    if !matched {
                        if should_emit(*kind, false, false) {
                            fold_group_row(
                                group_by,
                                aggs,
                                &probe_columns,
                                &bt,
                                row,
                                None,
                                &mut group_index,
                                &mut group_keys,
                                &mut accumulators,
                            );
                        }
                    } else if matches!(kind, JoinKind::Semi) {
                        // Mirrors `Opcode::HashProbe`'s own `Semi` handling:
                        // at most one contribution per probe row, with every
                        // `Payload` source NULL (semi-joins never surface
                        // the build side's columns).
                        fold_group_row(
                            group_by,
                            aggs,
                            &probe_columns,
                            &bt,
                            row,
                            None,
                            &mut group_index,
                            &mut group_keys,
                            &mut accumulators,
                        );
                    }
                }

                for (i, (_src, dst)) in group_by.iter().enumerate() {
                    let column: Vec<Value> = group_keys.iter().map(|k| k[i].clone()).collect();
                    self.registers.insert(*dst, Arc::new(column));
                }
                for (i, (func, src)) in aggs.iter().enumerate() {
                    let is_count_star = src.is_none();
                    let result: Vec<Value> = accumulators[i]
                        .iter()
                        .map(|acc| acc.finalize(*func, is_count_star))
                        .collect();
                    self.registers.insert(agg_dst[i], Arc::new(result));
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
                // #265: see the comment on `Map`'s same call.
                self.resolve_selection(opcode)?;
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
                )?;
                self.registers.insert(*dst, Arc::new(result));
            }
            Opcode::Emit { registers } => {
                // #262: Emit is terminal for these registers, so remove
                // each column from `self.registers` instead of borrowing
                // and cloning every cell into the output rows. A register
                // listed more than once in `registers` (e.g. `SELECT a,
                // a`) is removed on its first occurrence and `Arc::clone`d
                // (a refcount bump, not a cell copy) for the repeats.
                let mut cols: Vec<Arc<Vec<Value>>> = Vec::with_capacity(registers.len());
                for r in registers.iter() {
                    let owned = if let Some(existing) = self.registers.remove(r) {
                        existing
                    } else {
                        cols.iter()
                            .zip(registers.iter())
                            .find(|(_, seen_r)| *seen_r == r)
                            .map(|(col, _)| Arc::clone(col))
                            .ok_or(VmError::UnknownRegister {
                                opcode,
                                register: *r,
                            })?
                    };
                    cols.push(owned);
                }
                let base_len =
                    cols.first()
                        .map(|c| c.len())
                        .ok_or_else(|| VmError::MalformedProgram {
                            opcode,
                            reason: "emit has no registers".to_string(),
                        })?;
                for c in &cols {
                    if c.len() != base_len {
                        return Err(VmError::RegisterLengthMismatch { opcode });
                    }
                }
                // #265: lazily resolves any pending selection, indexing
                // straight into the (still uncompacted) emitted columns
                // instead of `Filter` having eagerly compacted every live
                // register up front.
                let selection = self.selection.take();
                if let Some(sel) = &selection {
                    if sel.base_len != base_len {
                        return Err(VmError::RegisterLengthMismatch { opcode });
                    }
                }
                // #436: build columns directly -- no per-row `Vec`, no
                // row-major transpose. `Vm::output` is column-major
                // (`QueryOutput`), so each of `cols` becomes exactly one
                // output column, moved or extended in one shot instead of
                // scattering its values into one `Vec` per row.
                let mut columns: Vec<Vec<Value>> = Vec::with_capacity(cols.len());
                for col in cols {
                    let column: Vec<Value> = if let Some(sel) = &selection {
                        // A selected subset can't be moved out of the
                        // `Arc` without leaving the unselected cells
                        // behind, so this always clones.
                        sel.indices
                            .iter()
                            .map(|&idx| col[idx as usize].clone())
                            .collect()
                    } else {
                        // #264: registers built fresh by this step (Map,
                        // Reduce, ...) hold the only strong reference, so
                        // `try_unwrap` moves the whole column out for
                        // free; a register `Arc::clone`d straight from
                        // the batch (a bare `LoadColumn` with no
                        // transform, or a repeated register above) is
                        // still shared with the batch/another emitted
                        // column, so it's cloned instead.
                        Arc::try_unwrap(col).unwrap_or_else(|shared| (*shared).clone())
                    };
                    columns.push(column);
                }
                self.output.extend(QueryOutput::new(columns));
            }
            // Meaningful only as loop markers interpreted by `run` --
            // and `Combine`/`Sort`/`Limit` are the cross-segment
            // sequential phase applied once by `crate::vm::engine::run`,
            // never inside a single segment. `ScanSource` is likewise a
            // no-op here: resolving it into a `Batch` happens above the
            // per-segment `Vm`, in `crate::vm::engine` (#385) -- this
            // ticket (#384) only defines the opcode/type, no emitted
            // program references it yet.
            Opcode::Scan
            | Opcode::NextSegment { .. }
            | Opcode::Halt
            | Opcode::Combine { .. }
            | Opcode::Sort { .. }
            | Opcode::Limit { .. }
            | Opcode::ScanSource { .. } => {}
        }
        Ok(())
    }
}

/// Ported 1:1 from column-rs's private `compute_window` (its only prior
/// implementation) -- partitions `0..num_rows` by `partition_cols`' tuple
/// (stringified, same non-NULL-safe convention as [`Opcode::GroupReduce`]'s
/// grouping), sorts each partition by `order_cols` via [`compare_for_order`],
/// then computes `func` per row within its partition.
#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "every column slice holds `num_rows` values and every index in `indices`/`partitions` was drawn from `0..num_rows`; `pos + 1` and the running counters are bounded by `num_rows`"
)]
fn compute_window(
    func: WindowFunc,
    offset: Option<i64>,
    partition_cols: &[&[Value]],
    order_cols: &[(&[Value], bool)],
    arg_col: Option<&[Value]>,
    num_rows: usize,
) -> Result<Vec<Value>> {
    // #266: a typed key (reusing GroupReduce's `GroupKey`, #263) instead
    // of stringifying+joining every partition column per row -- NULLs
    // still group together, matching `PARTITION BY`'s `GROUP BY`-like
    // semantics.
    let mut partitions: HashMap<GroupKey, Vec<usize>> = HashMap::new();
    let mut partition_order: Vec<GroupKey> = Vec::new();
    for row in 0..num_rows {
        let key = GroupKey(partition_cols.iter().map(|c| c[row].clone()).collect());
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
                    output[row] = Value::Int(len_to_i64(pos + 1));
                }
            }
            WindowFunc::Rank | WindowFunc::DenseRank => {
                let mut rank = 0i64;
                let mut dense = 0i64;
                let mut prev: Option<usize> = None;
                for (pos, &row) in indices.iter().enumerate() {
                    let is_new = match prev {
                        None => true,
                        // #266: reuses `compare_for_order`'s (now
                        // allocation-free for Str/Str and numeric pairs)
                        // ordering instead of a separate stringify-and-
                        // compare -- direction doesn't matter for an
                        // equality check, so `false` is arbitrary.
                        Some(prev_row) => order_cols.iter().any(|(col, _)| {
                            compare_for_order(&col[row], &col[prev_row], false)
                                != std::cmp::Ordering::Equal
                        }),
                    };
                    if is_new {
                        rank = len_to_i64(pos + 1);
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
                let Some(arg) = arg_col else {
                    return Err(VmError::MissingWindowArgument {
                        opcode: "Window",
                        func,
                    });
                };
                for (pos, &row) in indices.iter().enumerate() {
                    let pos = len_to_i64(pos);
                    let target = if func == WindowFunc::Lag {
                        pos - offset
                    } else {
                        pos + offset
                    };
                    // A negative or past-the-end target is out of frame
                    // -> NULL; `try_from` folds the `>= 0` check into the
                    // `get`.
                    output[row] = usize::try_from(target)
                        .ok()
                        .and_then(|t| indices.get(t))
                        .map_or(Value::Null, |&r| arg[r].clone());
                }
            }
            WindowFunc::FirstValue => {
                let Some(arg) = arg_col else {
                    return Err(VmError::MissingWindowArgument {
                        opcode: "Window",
                        func,
                    });
                };
                if let Some(&first) = indices.first() {
                    let v = arg[first].clone();
                    for &row in &indices {
                        output[row] = v.clone();
                    }
                }
            }
            WindowFunc::LastValue => {
                let Some(arg) = arg_col else {
                    return Err(VmError::MissingWindowArgument {
                        opcode: "Window",
                        func,
                    });
                };
                for &row in &indices {
                    output[row] = arg[row].clone();
                }
            }
            WindowFunc::Sum | WindowFunc::Avg | WindowFunc::Count => {
                // No ORDER BY: the frame is the whole partition -- one aggregate value for every row.
                if order_cols.is_empty() {
                    let agg = whole_partition_aggregate(func, arg_col, &indices)?;
                    for &row in &indices {
                        output[row] = agg.clone();
                    }
                } else {
                    let mut running_sum = 0.0;
                    let mut running_count = 0i64;
                    for &row in &indices {
                        // Running frame: every row up to and including this one counts.
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
                                // AVG over an empty running frame is NULL, never a division by zero.
                                if running_count > 0 {
                                    Value::Float(running_sum / running_count as f64)
                                } else {
                                    Value::Null
                                }
                            }
                            other => {
                                return Err(VmError::UnsupportedOp {
                                    opcode: "Window",
                                    op: format!("{other:?} as a running aggregate"),
                                })
                            }
                        };
                    }
                }
            }
        }
    }
    Ok(output)
}

/// `SUM`/`AVG`/`COUNT OVER (PARTITION BY ...)` with no `ORDER BY`: the
/// default frame is the whole partition, so every row in it gets the same
/// aggregate value.
#[allow(
    clippy::indexing_slicing,
    reason = "`indices` was drawn from `0..num_rows` by `compute_window`, and `arg_col` holds `num_rows` values"
)]
fn whole_partition_aggregate(
    func: WindowFunc,
    arg_col: Option<&[Value]>,
    indices: &[usize],
) -> Result<Value> {
    if func == WindowFunc::Count {
        let count = match arg_col {
            Some(a) => indices
                .iter()
                .filter(|&&row| !matches!(a[row], Value::Null))
                .count(),
            None => indices.len(),
        };
        return Ok(Value::Int(len_to_i64(count)));
    }
    let values: Vec<f64> = indices
        .iter()
        .filter_map(|&row| arg_col.and_then(|a| a[row].as_f64()))
        .collect();
    if values.is_empty() {
        return Ok(Value::Null);
    }
    match func {
        WindowFunc::Sum => Ok(Value::Float(values.iter().sum())),
        WindowFunc::Avg => Ok(Value::Float(
            values.iter().sum::<f64>() / values.len() as f64,
        )),
        other => Err(VmError::UnsupportedOp {
            opcode: "Window",
            op: format!("{other:?} as a partition aggregate"),
        }),
    }
}

fn reduce_count_star(func: AggFunc, num_rows: usize) -> Value {
    // COUNT(*) is the one aggregate that counts rows, not non-NULL values.
    match func {
        AggFunc::Count => Value::Int(len_to_i64(num_rows)),
        _ => Value::Null,
    }
}

/// Fast path for [`Opcode::Reduce`]'s global (non-grouped) aggregate
/// when `src` is a typed `Int`/`Float` [`Column`] (#130 child 4 slice 3,
/// db-core#433): iterates the packed buffer + validity bitmap directly
/// instead of boxing every value into `Value` and coercing through
/// `Value::as_f64`. Matches [`reduce_values`]'s output shape exactly --
/// `Sum`/`Avg`/`Min`/`Max` always produce `Value::Float` (even over
/// `Int` data), `Count` stays `Value::Int`, and an all-NULL/all-invalid
/// column yields `Value::Null` for `Sum`/`Avg`/`Min`/`Max` (never `0`).
/// `None` when `column` isn't `Int`/`Float`, so the caller falls back to
/// [`reduce_values`] over the materialized register.
fn typed_reduce_values(func: AggFunc, column: &Column) -> Option<Value> {
    if !matches!(column, Column::Int { .. } | Column::Float { .. }) {
        return None;
    }
    let len = column.len();
    let non_null = || (0..len).filter_map(|i| column_num_at(column, i).map(|(x, _)| x));
    Some(match func {
        AggFunc::Count => Value::Int(len_to_i64((0..len).filter(|&i| !column.is_null(i)).count())),
        AggFunc::Sum => {
            let (sum, count) =
                non_null().fold((0.0, 0usize), |(s, c), x| (s + x, c.saturating_add(1)));
            if count == 0 {
                Value::Null
            } else {
                Value::Float(sum)
            }
        }
        AggFunc::Avg => {
            let (sum, count) =
                non_null().fold((0.0, 0usize), |(s, c), x| (s + x, c.saturating_add(1)));
            if count == 0 {
                Value::Null
            } else {
                Value::Float(sum / count as f64)
            }
        }
        AggFunc::Min => non_null()
            .fold(None, |acc: Option<f64>, v| {
                Some(acc.map_or(v, |a| a.min(v)))
            })
            .map_or(Value::Null, Value::Float),
        AggFunc::Max => non_null()
            .fold(None, |acc: Option<f64>, v| {
                Some(acc.map_or(v, |a| a.max(v)))
            })
            .map_or(Value::Null, Value::Float),
    })
}

fn reduce_values(func: AggFunc, values: &[Value]) -> Value {
    let non_null: Vec<f64> = values.iter().filter_map(Value::as_f64).collect();
    match func {
        AggFunc::Count => Value::Int(len_to_i64(
            values.iter().filter(|v| !matches!(v, Value::Null)).count(),
        )),
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
    // `IsNull`/`IsNotNull` must observe a `Null` operand, so they run
    // before the null-propagation rule below.
    if matches!(op, MapOp::IsNull) {
        return Value::Bool(matches!(a, Value::Null));
    }
    if matches!(op, MapOp::IsNotNull) {
        return Value::Bool(!matches!(a, Value::Null));
    }
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Value::Null;
    }
    use std::cmp::Ordering::{Equal, Greater, Less};
    match op {
        MapOp::Add => arithmetic(op, a, b, |x, y| x + y),
        MapOp::Sub => arithmetic(op, a, b, |x, y| x - y),
        MapOp::Mul => arithmetic(op, a, b, |x, y| x * y),
        MapOp::Div => arithmetic(op, a, b, |x, y| x / y),
        MapOp::Eq => comparison(a, b, |o| o == Some(Equal)),
        MapOp::Ne => comparison(a, b, |o| o != Some(Equal)),
        MapOp::Lt => comparison(a, b, |o| o == Some(Less)),
        MapOp::Le => comparison(a, b, |o| matches!(o, Some(Less | Equal))),
        MapOp::Gt => comparison(a, b, |o| o == Some(Greater)),
        MapOp::Ge => comparison(a, b, |o| matches!(o, Some(Greater | Equal))),
        MapOp::And => Value::Bool(as_bool(a) && as_bool(b)),
        MapOp::Or => Value::Bool(as_bool(a) || as_bool(b)),
        MapOp::Not => Value::Bool(!as_bool(a)),
        MapOp::Concat => Value::Str(Cow::Owned(format!("{a}{b}"))),
        MapOp::Neg => match a {
            // `-i64::MIN` has no integer representation; SQLite promotes
            // that one case to REAL rather than overflowing.
            Value::Int(v) => v
                .checked_neg()
                .map_or(Value::Float(-(*v as f64)), Value::Int),
            Value::Float(v) => Value::Float(-v),
            _ => Value::Null,
        },
        // Already answered by the early returns above (they must see
        // `Null`); the same semantics here keep this match total without
        // an `unreachable!` the qualified subset forbids.
        MapOp::IsNull => Value::Bool(matches!(a, Value::Null)),
        MapOp::IsNotNull => Value::Bool(!matches!(a, Value::Null)),
        MapOp::MaskIf => {
            // MaskIf keeps `a` wherever the predicate register is true.
            if matches!(b, Value::Bool(true)) {
                a.clone()
            } else {
                Value::Null
            }
        }
        MapOp::Like { negated } => {
            let matched = crate::functions::like_match(&a.to_string(), &b.to_string(), None);
            Value::Bool(matched != negated)
        }
        MapOp::Glob { negated } => {
            let matched = crate::functions::glob_match(&a.to_string(), &b.to_string());
            Value::Bool(matched != negated)
        }
    }
}

/// `a op b` for the four arithmetic operators: `Int` when both operands
/// are `Int` and the result is exact (`Div` always yields `Float`).
fn arithmetic(op: MapOp, a: &Value, b: &Value, f: impl Fn(f64, f64) -> f64) -> Value {
    let (Some(x), Some(y)) = (a.as_f64(), b.as_f64()) else {
        return Value::Null;
    };
    let result = f(x, y);
    if matches!(a, Value::Int(_)) && matches!(b, Value::Int(_)) && op != MapOp::Div {
        // `as` from `f64` saturates at the `i64` bounds and maps NaN to
        // 0 -- the intended overflow behavior for Int arithmetic here.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "saturating f64 -> i64 is the documented overflow semantics"
        )]
        Value::Int(result as i64)
    } else {
        Value::Float(result)
    }
}

fn comparison(a: &Value, b: &Value, pred: impl Fn(Option<std::cmp::Ordering>) -> bool) -> Value {
    Value::Bool(pred(compare_values(a, b)))
}

fn as_bool(v: &Value) -> bool {
    matches!(v, Value::Bool(true))
}

/// Row `i` of a typed `Int`/`Float` [`Column`] as `(value, is_int)`, or
/// `None` if `i` is NULL, out of range, or `column` isn't `Int`/`Float`.
/// A plain function (not a lifetime-carrying wrapper type) because
/// `vm/batch.rs` is in the qualified subset (`make check-mvl-limit`),
/// which allows only function-scoped lifetime elision.
fn column_num_at(column: &Column, i: usize) -> Option<(f64, bool)> {
    match column {
        Column::Int { data, valid } => {
            if !valid.get(i) {
                return None;
            }
            #[allow(
                clippy::cast_precision_loss,
                reason = "matches Value::as_f64's existing `*v as f64` widening for Int"
            )]
            data.get(i).map(|&x| (x as f64, true))
        }
        Column::Float { data, valid } => {
            if !valid.get(i) {
                return None;
            }
            data.get(i).map(|&x| (x, false))
        }
        _ => None,
    }
}

/// `a op b` for [`Vm::typed_arithmetic`]'s four arithmetic ops -- the
/// same four [`arithmetic`] handles, applied directly to `f64` since the
/// typed fast path already knows both operands are numeric and non-NULL.
/// Total over [`MapOp`] (never actually reached for a non-arithmetic op:
/// [`Vm::typed_arithmetic`] guards on that before calling) rather than
/// `unreachable!`, which the qualified subset forbids.
fn arith_op(op: MapOp, x: f64, y: f64) -> f64 {
    match op {
        MapOp::Add => x + y,
        MapOp::Sub => x - y,
        MapOp::Mul => x * y,
        MapOp::Div => x / y,
        _ => 0.0,
    }
}

/// `Some(s)` if every value in `values` is the same `Value::Str(s)`
/// (typically a `LoadConst`-broadcast register); `None` if `values` is
/// empty, holds anything else, or the strings differ. Used by
/// [`Vm::dict_literal_compare`] to recognize a comparison against a
/// literal.
fn single_str_literal(values: &[Value]) -> Option<&str> {
    let Value::Str(first) = values.first()? else {
        return None;
    };
    let first = first.as_ref();
    values
        .iter()
        .all(|v| matches!(v, Value::Str(s) if s.as_ref() == first))
        .then_some(first)
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
    fn query_output_from_rows_round_trips_through_into_rows() {
        let rows = vec![
            vec![Value::Int(1), Value::Str("a".into())],
            vec![Value::Int(2), Value::Str("b".into())],
        ];
        let output = QueryOutput::from_rows(rows.clone());
        assert_eq!(output.num_columns(), 2);
        assert_eq!(output.num_rows(), 2);
        assert_eq!(output.clone().into_rows(), rows);
        // #436: `PartialEq<Vec<Vec<Value>>>` lets a `QueryOutput` compare
        // directly against a row-major literal, no explicit `into_rows()`.
        assert_eq!(output, rows);
    }

    #[test]
    fn query_output_from_rows_of_empty_vec_has_no_columns() {
        let output = QueryOutput::from_rows(Vec::new());
        assert_eq!(output.num_columns(), 0);
        assert_eq!(output.num_rows(), 0);
        assert!(output.is_empty());
    }

    #[test]
    fn query_output_extend_concatenates_column_wise() {
        let mut a = QueryOutput::new(vec![vec![Value::Int(1)], vec![Value::Int(10)]]);
        let b = QueryOutput::new(vec![vec![Value::Int(2)], vec![Value::Int(20)]]);
        a.extend(b);
        assert_eq!(a.num_rows(), 2);
        assert_eq!(
            a,
            vec![
                vec![Value::Int(1), Value::Int(10)],
                vec![Value::Int(2), Value::Int(20)],
            ]
        );
    }

    #[test]
    fn query_output_extend_onto_a_default_output_adopts_the_other_columns() {
        // The shape `Opcode::Emit` and `run_parallel` rely on: the very
        // first `extend` call onto a brand new (zero-column) `QueryOutput`
        // must adopt the incoming columns, not try to zip zero columns
        // against N and silently drop everything.
        let mut acc = QueryOutput::default();
        acc.extend(QueryOutput::new(vec![vec![Value::Int(1), Value::Int(2)]]));
        assert_eq!(acc, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
    }

    #[test]
    fn query_output_truncate_shortens_every_column_equally() {
        let mut output = QueryOutput::new(vec![
            vec![Value::Int(1), Value::Int(2), Value::Int(3)],
            vec![Value::Int(10), Value::Int(20), Value::Int(30)],
        ]);
        output.truncate(2);
        assert_eq!(output.num_rows(), 2);
        assert_eq!(
            output,
            vec![
                vec![Value::Int(1), Value::Int(10)],
                vec![Value::Int(2), Value::Int(20)],
            ]
        );
    }

    #[test]
    fn agg_func_from_name_valid_case_insensitive() {
        for (name, expected) in [
            ("count", AggFunc::Count),
            ("COUNT", AggFunc::Count),
            ("Count", AggFunc::Count),
            ("sum", AggFunc::Sum),
            ("SUM", AggFunc::Sum),
            ("avg", AggFunc::Avg),
            ("AVG", AggFunc::Avg),
            ("min", AggFunc::Min),
            ("MIN", AggFunc::Min),
            ("max", AggFunc::Max),
            ("MAX", AggFunc::Max),
        ] {
            assert_eq!(AggFunc::from_name(name), Some(expected), "name = {name}");
        }
    }

    #[test]
    fn agg_func_from_name_invalid() {
        assert_eq!(AggFunc::from_name("bogus"), None);
        assert_eq!(AggFunc::from_name(""), None);
        assert_eq!(AggFunc::from_name("counter"), None);
    }

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
    fn load_column_shares_the_batch_s_arc_instead_of_copying() {
        // #264: an unmodified column's register is the same allocation as
        // the batch's column (an `Arc::clone`, not a per-cell copy).
        let batch =
            Batch::new(3).with_column("id", vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        let batch_column_ptr = batch.columns["id"].as_ptr();
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[Opcode::LoadColumn {
                reg: 0,
                column: "id".into(),
            }],
        )
        .unwrap();
        assert!(std::ptr::eq(
            vm.register(0).unwrap().as_ptr(),
            batch_column_ptr
        ));
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
    fn load_column_reads_a_typed_column_into_matching_values() {
        // #429: a `Batch::with_typed_column` column materializes into the
        // same `Value`s a `Vec<Value>`-backed column would, so every
        // opcode not yet ported to `Column` dispatch sees no difference.
        let batch = Batch::new(3).with_typed_column(
            "id",
            Column::from(vec![Value::Int(1), Value::Null, Value::Int(3)]),
        );
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
            &[Value::Int(1), Value::Null, Value::Int(3)]
        );
        assert!(matches!(vm.typed_register(0), Some(Column::Int { .. })));
    }

    #[test]
    fn filter_on_a_typed_bool_predicate_matches_the_value_path() {
        // #429: Filter's typed-Bool fast path (reading the Column's
        // bitmap+data directly) must keep only rows the equivalent
        // `Vec<Value>` path would -- including a NULL predicate reading
        // as "not kept", same as `Value::Bool(true)` matching does.
        let batch = Batch::new(4)
            .with_column(
                "id",
                vec![Value::Int(1), Value::Int(2), Value::Int(3), Value::Int(4)],
            )
            .with_typed_column(
                "keep",
                Column::from(vec![
                    Value::Bool(true),
                    Value::Bool(false),
                    Value::Null,
                    Value::Bool(true),
                ]),
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
                    column: "keep".into(),
                },
                Opcode::Filter { predicate: 1 },
                Opcode::Emit {
                    registers: vec![0].into(),
                },
            ],
        )
        .unwrap();
        assert_eq!(
            vm.take_output(),
            vec![vec![Value::Int(1)], vec![Value::Int(4)]]
        );
    }

    #[test]
    fn map_eq_on_dict_column_compares_codes_against_a_literal() {
        // #399/#429: `= 'kern'` on a Dict column must compare dictionary
        // codes, not decode every row back to a String, and must agree
        // with the equivalent `Vec<Value>`/`Str` comparison.
        let dict: Vec<std::sync::Arc<str>> = vec!["kern".into(), "user".into()];
        let tag_column = Column::Dict {
            dict,
            indices: vec![0, 1, 0],
            valid: crate::vm::column::Bitmap::from_bools([true, true, false].into_iter()),
        };
        let batch = Batch::new(3).with_typed_column("tag", tag_column);
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[
                Opcode::LoadColumn {
                    reg: 0,
                    column: "tag".into(),
                },
                Opcode::LoadConst {
                    reg: 1,
                    value: Value::Str("kern".into()),
                },
                Opcode::Map {
                    dst: 2,
                    op: MapOp::Eq,
                    a: 0,
                    b: 1,
                },
            ],
        )
        .unwrap();
        assert_eq!(
            vm.register(2).unwrap(),
            &[Value::Bool(true), Value::Bool(false), Value::Null]
        );

        // Differential check: the same query over an equivalent
        // `Vec<Value>`/`Str` column must agree exactly.
        let str_batch = Batch::new(3).with_column(
            "tag",
            vec![
                Value::Str("kern".into()),
                Value::Str("user".into()),
                Value::Null,
            ],
        );
        let mut str_vm = Vm::new();
        str_vm
            .execute(
                &str_batch,
                &[
                    Opcode::LoadColumn {
                        reg: 0,
                        column: "tag".into(),
                    },
                    Opcode::LoadConst {
                        reg: 1,
                        value: Value::Str("kern".into()),
                    },
                    Opcode::Map {
                        dst: 2,
                        op: MapOp::Eq,
                        a: 0,
                        b: 1,
                    },
                ],
            )
            .unwrap();
        assert_eq!(vm.register(2).unwrap(), str_vm.register(2).unwrap());
    }

    #[test]
    fn map_eq_on_dict_column_against_a_literal_not_in_the_dictionary() {
        let dict: Vec<std::sync::Arc<str>> = vec!["kern".into(), "user".into()];
        let tag_column = Column::Dict {
            dict,
            indices: vec![0, 1],
            valid: crate::vm::column::Bitmap::new(2, true),
        };
        let batch = Batch::new(2).with_typed_column("tag", tag_column);
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[
                Opcode::LoadColumn {
                    reg: 0,
                    column: "tag".into(),
                },
                Opcode::LoadConst {
                    reg: 1,
                    value: Value::Str("nope".into()),
                },
                Opcode::Map {
                    dst: 2,
                    op: MapOp::Ne,
                    a: 0,
                    b: 1,
                },
            ],
        )
        .unwrap();
        assert_eq!(
            vm.register(2).unwrap(),
            &[Value::Bool(true), Value::Bool(true)]
        );
    }

    #[test]
    fn map_add_on_two_typed_int_columns_matches_the_value_path() {
        // #431: two typed Int columns must produce the same Int result,
        // with the same NULL propagation, as the equivalent Vec<Value>
        // path, and the destination register should itself be typed.
        let batch = Batch::new(3)
            .with_typed_column(
                "a",
                Column::from(vec![Value::Int(1), Value::Int(2), Value::Null]),
            )
            .with_typed_column(
                "b",
                Column::from(vec![Value::Int(10), Value::Null, Value::Int(30)]),
            );
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[
                Opcode::LoadColumn {
                    reg: 0,
                    column: "a".into(),
                },
                Opcode::LoadColumn {
                    reg: 1,
                    column: "b".into(),
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
        assert_eq!(
            vm.register(2).unwrap(),
            &[Value::Int(11), Value::Null, Value::Null]
        );
        assert!(matches!(vm.typed_register(2), Some(Column::Int { .. })));

        let str_batch = Batch::new(3)
            .with_column("a", vec![Value::Int(1), Value::Int(2), Value::Null])
            .with_column("b", vec![Value::Int(10), Value::Null, Value::Int(30)]);
        let mut str_vm = Vm::new();
        str_vm
            .execute(
                &str_batch,
                &[
                    Opcode::LoadColumn {
                        reg: 0,
                        column: "a".into(),
                    },
                    Opcode::LoadColumn {
                        reg: 1,
                        column: "b".into(),
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
        assert_eq!(vm.register(2).unwrap(), str_vm.register(2).unwrap());
    }

    #[test]
    fn map_div_on_typed_int_columns_promotes_to_float() {
        let batch = Batch::new(2)
            .with_typed_column("a", Column::from(vec![Value::Int(7), Value::Int(9)]))
            .with_typed_column("b", Column::from(vec![Value::Int(2), Value::Int(3)]));
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[
                Opcode::LoadColumn {
                    reg: 0,
                    column: "a".into(),
                },
                Opcode::LoadColumn {
                    reg: 1,
                    column: "b".into(),
                },
                Opcode::Map {
                    dst: 2,
                    op: MapOp::Div,
                    a: 0,
                    b: 1,
                },
            ],
        )
        .unwrap();
        assert_eq!(
            vm.register(2).unwrap(),
            &[Value::Float(3.5), Value::Float(3.0)]
        );
        assert!(matches!(vm.typed_register(2), Some(Column::Float { .. })));
    }

    #[test]
    fn map_add_on_typed_int_and_typed_float_columns_promotes_to_float() {
        let batch = Batch::new(2)
            .with_typed_column("a", Column::from(vec![Value::Int(1), Value::Int(2)]))
            .with_typed_column(
                "b",
                Column::from(vec![Value::Float(0.5), Value::Float(1.5)]),
            );
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[
                Opcode::LoadColumn {
                    reg: 0,
                    column: "a".into(),
                },
                Opcode::LoadColumn {
                    reg: 1,
                    column: "b".into(),
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
        assert_eq!(
            vm.register(2).unwrap(),
            &[Value::Float(1.5), Value::Float(3.5)]
        );
    }

    #[test]
    fn map_add_falls_back_to_the_value_path_when_one_operand_is_a_broadcast_const() {
        // #431's fast path is scoped to two typed columns; a `LoadConst`
        // broadcast is a plain `Vec<Value>` register, so this must still
        // produce the correct result via the general path.
        let batch =
            Batch::new(2).with_typed_column("a", Column::from(vec![Value::Int(1), Value::Int(2)]));
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[
                Opcode::LoadColumn {
                    reg: 0,
                    column: "a".into(),
                },
                Opcode::LoadConst {
                    reg: 1,
                    value: Value::Int(10),
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
        assert_eq!(vm.register(2).unwrap(), &[Value::Int(11), Value::Int(12)]);
    }

    #[test]
    fn typed_column_survives_a_resolved_selection_via_the_materialized_register() {
        // #429: once `resolve_selection` compacts registers (here via
        // `Map` after a `Filter`), the typed shadow is cleared -- but
        // the always-populated `registers` copy keeps everything correct,
        // just without the fast path from then on.
        let batch = Batch::new(3)
            .with_typed_column(
                "n",
                Column::from(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
            )
            .with_column(
                "keep",
                vec![Value::Bool(true), Value::Bool(false), Value::Bool(true)],
            );
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[
                Opcode::LoadColumn {
                    reg: 0,
                    column: "n".into(),
                },
                Opcode::LoadColumn {
                    reg: 1,
                    column: "keep".into(),
                },
                Opcode::Filter { predicate: 1 },
                Opcode::LoadConst {
                    reg: 2,
                    value: Value::Int(1),
                },
                Opcode::Map {
                    dst: 3,
                    op: MapOp::Add,
                    a: 0,
                    b: 2,
                },
                Opcode::Emit {
                    registers: vec![3].into(),
                },
            ],
        )
        .unwrap();
        assert_eq!(
            vm.take_output(),
            vec![vec![Value::Int(2)], vec![Value::Int(4)]]
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
    #[allow(non_snake_case)]
    fn mcdc__vm_batch_apply_map_op_0e991c46__v1_a_null_propagates() {
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
                    value: Value::Int(5),
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
    #[allow(non_snake_case)]
    fn mcdc__vm_batch_apply_map_op_0e991c46__v2_b_null_propagates() {
        let batch = Batch::new(1);
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[
                Opcode::LoadConst {
                    reg: 0,
                    value: Value::Int(5),
                },
                Opcode::LoadConst {
                    reg: 1,
                    value: Value::Null,
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
    #[allow(non_snake_case)]
    fn mcdc__vm_batch_apply_map_op_0e991c46__v3_neither_null_computes_result() {
        let batch = Batch::new(1);
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[
                Opcode::LoadConst {
                    reg: 0,
                    value: Value::Int(5),
                },
                Opcode::LoadConst {
                    reg: 1,
                    value: Value::Int(3),
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
        assert_eq!(vm.register(2).unwrap(), &[Value::Int(8)]);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_batch_arithmetic_f08e9d82__v1_both_int_non_div_stays_int() {
        let batch = Batch::new(1);
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
            ],
        )
        .unwrap();
        assert_eq!(vm.register(2).unwrap(), &[Value::Int(14)]);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_batch_arithmetic_f08e9d82__v2_a_not_int_promotes_to_float() {
        let batch = Batch::new(1);
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[
                Opcode::LoadConst {
                    reg: 0,
                    value: Value::Float(10.0),
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
            ],
        )
        .unwrap();
        assert_eq!(vm.register(2).unwrap(), &[Value::Float(14.0)]);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_batch_arithmetic_f08e9d82__v3_b_not_int_promotes_to_float() {
        let batch = Batch::new(1);
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
                    value: Value::Float(4.0),
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
        assert_eq!(vm.register(2).unwrap(), &[Value::Float(14.0)]);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_batch_arithmetic_f08e9d82__v4_div_promotes_to_float_even_with_two_ints() {
        let batch = Batch::new(1);
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
                    op: MapOp::Div,
                    a: 0,
                    b: 1,
                },
            ],
        )
        .unwrap();
        assert_eq!(vm.register(2).unwrap(), &[Value::Float(2.5)]);
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
            // `-i64::MIN` has no integer representation: promoted to REAL
            // (SQLite's behaviour) instead of overflowing.
            (Value::Int(i64::MIN), Value::Float(-(i64::MIN as f64))),
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
        vm.registers.insert(1, Arc::new(vec![Value::Int(1)]));
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
        let program = [
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
            Opcode::Emit {
                registers: vec![0, 1].into(),
            },
        ];
        vm.execute(&batch, &program).unwrap();
        assert_eq!(
            vm.take_output(),
            vec![
                vec![Value::Int(2), Value::Int(15)],
                vec![Value::Int(3), Value::Int(25)],
            ]
        );
    }

    #[test]
    fn filter_defers_compaction_until_a_consumer_resolves_it() {
        // #265: Filter alone doesn't touch registers -- it only records a
        // selection vector; a register is still the full, pre-filter
        // length until something (Emit here) resolves it.
        let batch = Batch::new(3)
            .with_column("id", vec![Value::Int(1), Value::Int(2), Value::Int(3)])
            .with_column(
                "keep",
                vec![Value::Bool(false), Value::Bool(true), Value::Bool(true)],
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
                    column: "keep".into(),
                },
                Opcode::Filter { predicate: 1 },
            ],
        )
        .unwrap();
        assert_eq!(
            vm.register(0).unwrap(),
            &[Value::Int(1), Value::Int(2), Value::Int(3)]
        );
    }

    #[test]
    fn a_second_filter_intersects_the_pending_selection() {
        // #265: two `Filter`s back to back (no consumer resolving the
        // first one's selection in between) intersect rather than the
        // second silently overwriting the first.
        let batch = Batch::new(4)
            .with_column(
                "id",
                vec![Value::Int(1), Value::Int(2), Value::Int(3), Value::Int(4)],
            )
            .with_column(
                "gt1",
                vec![
                    Value::Bool(false),
                    Value::Bool(true),
                    Value::Bool(true),
                    Value::Bool(true),
                ],
            )
            .with_column(
                "lt4",
                vec![
                    Value::Bool(true),
                    Value::Bool(true),
                    Value::Bool(true),
                    Value::Bool(false),
                ],
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
                    column: "gt1".into(),
                },
                Opcode::LoadColumn {
                    reg: 2,
                    column: "lt4".into(),
                },
                Opcode::Filter { predicate: 1 },
                Opcode::Filter { predicate: 2 },
                Opcode::Emit {
                    registers: vec![0].into(),
                },
            ],
        )
        .unwrap();
        assert_eq!(
            vm.take_output(),
            vec![vec![Value::Int(2)], vec![Value::Int(3)]]
        );
    }

    #[test]
    fn filter_length_mismatch_errors() {
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
        vm.registers.insert(1, Arc::new(vec![Value::Bool(true)]));
        let err = vm
            .step(&batch, &Opcode::Filter { predicate: 1 })
            .unwrap_err();
        assert_eq!(err, VmError::RegisterLengthMismatch { opcode: "Filter" });
    }

    #[test]
    fn filter_then_map_forces_compaction_and_stays_correct() {
        // #265: Map has no row-index concept, so a pending selection must
        // be fully resolved before it runs -- exercises `Map`'s
        // `resolve_selection` call, matching a real compiled program
        // (`SELECT a + b ... WHERE ...` runs `Map` after `Filter`).
        let batch = Batch::new(3)
            .with_column("a", vec![Value::Int(1), Value::Int(2), Value::Int(3)])
            .with_column("b", vec![Value::Int(10), Value::Int(20), Value::Int(30)])
            .with_column(
                "keep",
                vec![Value::Bool(false), Value::Bool(true), Value::Bool(true)],
            );
        let mut vm = Vm::new();
        vm.execute(
            &batch,
            &[
                Opcode::LoadColumn {
                    reg: 0,
                    column: "a".into(),
                },
                Opcode::LoadColumn {
                    reg: 1,
                    column: "b".into(),
                },
                Opcode::LoadColumn {
                    reg: 2,
                    column: "keep".into(),
                },
                Opcode::Filter { predicate: 2 },
                Opcode::Map {
                    dst: 3,
                    op: MapOp::Add,
                    a: 0,
                    b: 1,
                },
                Opcode::Emit {
                    registers: vec![3].into(),
                },
            ],
        )
        .unwrap();
        assert_eq!(
            vm.take_output(),
            vec![vec![Value::Int(22)], vec![Value::Int(33)]]
        );
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
    fn reduce_over_a_typed_int_column_matches_the_value_path() {
        // #433: a typed Int source register must produce identical
        // results, for every AggFunc, to the equivalent Vec<Value>
        // column -- including Count staying Int while the rest promote
        // to Float, and NULLs being skipped rather than zero-filled.
        let batch = Batch::new(4).with_typed_column(
            "amount",
            Column::from(vec![
                Value::Int(10),
                Value::Null,
                Value::Int(20),
                Value::Int(30),
            ]),
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
        assert!(matches!(vm.typed_register(0), Some(Column::Int { .. })));

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
    fn reduce_over_an_all_invalid_typed_column_is_null_not_zero() {
        let batch =
            Batch::new(2).with_typed_column("amount", Column::from(vec![Value::Null, Value::Null]));
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

    #[test]
    fn filter_then_group_reduce_resolves_the_pending_selection() {
        // #265: GroupReduce is one of the lazy resolvers -- it must index
        // through Filter's pending selection itself rather than assuming
        // its source registers are already compacted.
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
            )
            .with_column(
                "keep",
                vec![
                    Value::Bool(true),
                    Value::Bool(false),
                    Value::Bool(true),
                    Value::Bool(true),
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
                Opcode::LoadColumn {
                    reg: 2,
                    column: "keep".into(),
                },
                Opcode::Filter { predicate: 2 },
                Opcode::GroupReduce {
                    group_by: vec![0].into(),
                    aggs: vec![(AggFunc::Sum, Some(1))].into(),
                    agg_dst: vec![3].into(),
                },
            ],
        )
        .unwrap();
        // Row 1 (west, 5) is filtered out, so west's sum is just row 3's 15.
        assert_eq!(
            vm.register(0).unwrap(),
            &[Value::Str("east".into()), Value::Str("west".into())]
        );
        assert_eq!(
            vm.register(3).unwrap(),
            &[Value::Float(30.0), Value::Float(15.0)]
        );
    }

    #[test]
    fn group_reduce_groups_all_null_keys_together() {
        // Unlike a join key (`hash_probe_null_keys_never_match`), a
        // `GROUP BY` key groups every NULL into the same group (#263).
        let batch = Batch::new(4)
            .with_column(
                "region",
                vec![
                    Value::Null,
                    Value::Str("east".into()),
                    Value::Null,
                    Value::Null,
                ],
            )
            .with_column(
                "amount",
                vec![Value::Int(1), Value::Int(2), Value::Int(3), Value::Int(4)],
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
                    aggs: vec![(AggFunc::Sum, Some(1))].into(),
                    agg_dst: vec![2].into(),
                },
            ],
        )
        .unwrap();
        assert_eq!(
            vm.register(0).unwrap(),
            &[Value::Null, Value::Str("east".into())]
        );
        assert_eq!(
            vm.register(2).unwrap(),
            &[Value::Float(8.0), Value::Float(2.0)]
        );
    }

    #[test]
    fn filter_then_hash_build_resolves_the_pending_selection() {
        // #265: HashBuild is one of the lazy resolvers -- only its own
        // key/payload registers are indexed through the selection.
        let right = Batch::new(3)
            .with_column("rkey", vec![Value::Int(1), Value::Int(2), Value::Int(3)])
            .with_column(
                "rval",
                vec![
                    Value::Str("a".into()),
                    Value::Str("b".into()),
                    Value::Str("c".into()),
                ],
            )
            .with_column(
                "keep",
                vec![Value::Bool(true), Value::Bool(false), Value::Bool(true)],
            );
        let mut build_vm = Vm::new();
        build_vm
            .execute(
                &right,
                &[
                    Opcode::LoadColumn {
                        reg: 0,
                        column: "rkey".into(),
                    },
                    Opcode::LoadColumn {
                        reg: 1,
                        column: "rval".into(),
                    },
                    Opcode::LoadColumn {
                        reg: 2,
                        column: "keep".into(),
                    },
                    Opcode::Filter { predicate: 2 },
                    Opcode::HashBuild {
                        key_cols: vec![0].into(),
                        payload_cols: vec![1].into(),
                        table: 0,
                    },
                ],
            )
            .unwrap();

        let left =
            Batch::new(3).with_column("lkey", vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        let mut vm = Vm::with_join_tables(build_vm.join_tables());
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
                    kind: JoinKind::Inner,
                },
            ],
        )
        .unwrap();
        // key=2 was filtered out of the build side, so it never matches.
        assert_eq!(vm.register(0).unwrap(), &[Value::Int(1), Value::Int(3)]);
        assert_eq!(
            vm.register(1).unwrap(),
            &[Value::Str("a".into()), Value::Str("c".into())]
        );
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
    fn hash_probe_length_mismatch_errors() {
        let mut build_vm = Vm::new();
        build_vm
            .execute(
                &Batch::new(1).with_column("rkey", vec![Value::Int(1)]),
                &[
                    Opcode::LoadColumn {
                        reg: 0,
                        column: "rkey".into(),
                    },
                    Opcode::HashBuild {
                        key_cols: vec![0].into(),
                        payload_cols: vec![0].into(),
                        table: 0,
                    },
                ],
            )
            .unwrap();

        let batch = Batch::new(2).with_column("lkey", vec![Value::Int(1), Value::Int(2)]);
        let mut vm = Vm::with_join_tables(build_vm.join_tables());
        vm.execute(
            &batch,
            &[Opcode::LoadColumn {
                reg: 0,
                column: "lkey".into(),
            }],
        )
        .unwrap();
        // A second, mismatched-length live register alongside the key
        // column -- HashProbe's own reshape loop must catch this itself.
        vm.registers.insert(1, Arc::new(vec![Value::Int(1)]));
        let err = vm
            .step(
                &batch,
                &Opcode::HashProbe {
                    key_cols: vec![0].into(),
                    table: 0,
                    payload_dst: vec![2].into(),
                    kind: JoinKind::Inner,
                },
            )
            .unwrap_err();
        assert_eq!(
            err,
            VmError::RegisterLengthMismatch {
                opcode: "HashProbe"
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
    fn window_row_number_groups_all_null_partitions_together() {
        // #266: the typed GroupKey partition key must keep NULL == NULL
        // for PARTITION BY, same as GROUP BY -- two NULL-partition rows
        // are one partition, not each its own (which a NULL-poisoned
        // JoinKey-style equality would produce).
        let batch = Batch::new(3)
            .with_column(
                "part",
                vec![Value::Null, Value::Null, Value::Str("a".into())],
            )
            .with_column("ord", vec![Value::Int(1), Value::Int(2), Value::Int(1)]);
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
        assert_eq!(rows, vec![Value::Int(1), Value::Int(2), Value::Int(1)]);
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
    fn lag_lead_target_in_bounds_yields_source_value() {
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
        // LEAD at pos 0 with offset 1: target = 1, in bounds [0, 3).
        let lead = run_window(
            &batch,
            Opcode::Window {
                func: WindowFunc::Lead,
                arg: Some(2),
                offset: Some(1),
                partition_by: vec![].into(),
                order_by: vec![(1, false)].into(),
                dst: 10,
            },
        );
        assert_eq!(lead[0], Value::Str("y".into()));
    }

    #[test]
    fn lag_lead_target_below_zero_yields_null() {
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
        // LAG at pos 0 with offset 1: target = -1, fails `target >= 0`.
        let lag = run_window(
            &batch,
            Opcode::Window {
                func: WindowFunc::Lag,
                arg: Some(2),
                offset: Some(1),
                partition_by: vec![].into(),
                order_by: vec![(1, false)].into(),
                dst: 10,
            },
        );
        assert_eq!(lag[0], Value::Null);
    }

    #[test]
    fn lag_lead_target_at_or_past_len_yields_null() {
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
        // LEAD at pos 2 (last row) with offset 1: target = 3, fails
        // `target < n` (n = 3) even though `target >= 0` holds.
        let lead = run_window(
            &batch,
            Opcode::Window {
                func: WindowFunc::Lead,
                arg: Some(2),
                offset: Some(1),
                partition_by: vec![].into(),
                order_by: vec![(1, false)].into(),
                dst: 10,
            },
        );
        assert_eq!(lead[2], Value::Null);
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
    fn emit_repeated_register_clones_only_the_repeat() {
        let batches = vec![Batch::new(2).with_column("id", vec![Value::Int(1), Value::Int(2)])];
        let mut source = VecSource::new(batches);
        let mut vm = Vm::new();
        let program = vec![
            Opcode::Scan,
            Opcode::LoadColumn {
                reg: 0,
                column: "id".into(),
            },
            Opcode::Emit {
                registers: vec![0, 0].into(),
            },
            Opcode::NextSegment { loop_start: 1 },
            Opcode::Halt,
        ];
        let rows = vm.run(&mut source, &program).unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Int(1)],
                vec![Value::Int(2), Value::Int(2)],
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
        fn load(&self) -> Result<Arc<Batch>> {
            Ok(Arc::new(self.0.clone()))
        }
    }

    #[test]
    fn segment_load_shares_columns_instead_of_deep_copying() {
        // #264: `Batch::clone` (what `Segment::load` does here) is a
        // `HashMap`-of-`Arc` clone -- each load's columns are the same
        // allocation as the original, not a fresh per-cell copy.
        let segment = InMemorySegment(Batch::new(1).with_column("id", vec![Value::Int(1)]));
        let a = segment.load().unwrap();
        let b = segment.load().unwrap();
        assert!(Arc::ptr_eq(&a.columns["id"], &b.columns["id"]));
    }

    #[test]
    fn run_parallel_scans_all_segments_in_order() {
        let segments: Vec<InMemorySegment> = (0..8)
            .map(|i| InMemorySegment(Batch::new(1).with_column("id", vec![Value::Int(i)])))
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
        let rows = run_parallel(&segments, &program).unwrap().into_rows();
        let ids: Vec<i64> = rows
            .iter()
            .map(|r| match &r[0] {
                Value::Int(v) => *v,
                other => panic!("expected Value::Int, got {other:?}"),
            })
            .collect();
        assert_eq!(ids, (0..8).collect::<Vec<_>>());
    }

    #[test]
    fn run_parallel_applies_filter_per_segment() {
        let segments: Vec<InMemorySegment> = vec![
            InMemorySegment(
                Batch::new(2).with_column("amount", vec![Value::Int(5), Value::Int(15)]),
            ),
            InMemorySegment(
                Batch::new(2).with_column("amount", vec![Value::Int(25), Value::Int(3)]),
            ),
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
        let segments: Vec<InMemorySegment> = vec![
            InMemorySegment(
                Batch::new(3)
                    .with_column("amount", vec![Value::Int(5), Value::Int(15), Value::Null]),
            ),
            InMemorySegment(Batch::new(3).with_column(
                "amount",
                vec![Value::Int(25), Value::Int(3), Value::Int(20)],
            )),
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
        let segments: Vec<InMemorySegment> = vec![InMemorySegment(Batch::new(4).with_column(
            "amount",
            vec![Value::Int(5), Value::Null, Value::Int(1), Value::Int(9)],
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
        let segments: Vec<InMemorySegment> = vec![InMemorySegment(
            Batch::new(2).with_column("amount", vec![Value::Int(2), Value::Int(1)]),
        )];
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

    #[test]
    fn run_morsels_rebalances_across_skewed_segment_costs() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::{Duration, Instant};

        // More segments than worker threads, with wildly uneven simulated
        // load: a static per-thread split (segments.len() / num_threads
        // chunks handed out up front) would let one thread get stuck with
        // all the expensive segments while others idle -- wall time would
        // then track the *worst single thread's total*, not the sum spread
        // evenly. A dynamic pull keeps every thread busy until the queue is
        // drained, so wall time tracks (total work / thread count) instead.
        let num_threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        if num_threads < 2 {
            return; // rebalancing needs >1 worker to be observable
        }

        let costs_ms: Vec<u64> = (0..num_threads * 4)
            .map(|i| if i % num_threads == 0 { 40 } else { 2 })
            .collect();
        let total_ms: u64 = costs_ms.iter().sum();
        let concurrent_peak = AtomicUsize::new(0);
        let concurrent_now = AtomicUsize::new(0);

        let start = Instant::now();
        run_morsels(&costs_ms, |&cost_ms| {
            let n = concurrent_now.fetch_add(1, Ordering::SeqCst) + 1;
            concurrent_peak.fetch_max(n, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(cost_ms));
            concurrent_now.fetch_sub(1, Ordering::SeqCst);
        });
        let elapsed = start.elapsed();

        // A static split (one contiguous chunk per thread, decided before
        // any work starts) would serialize every "40ms" segment onto
        // whichever thread(s) statically own that chunk, so a bad split
        // could take close to the full `total_ms` sum. Dynamic pulling
        // bounds wall time near `total_ms / num_threads` (plus scheduling
        // slack) because idle threads immediately grab the next unclaimed
        // segment instead of sitting on a fixed assignment.
        let balanced_upper_bound = Duration::from_millis(total_ms / num_threads as u64 + 60);
        assert!(
            elapsed <= balanced_upper_bound,
            "expected dynamic rebalancing to finish within {balanced_upper_bound:?}, took {elapsed:?} (total work {total_ms}ms over {num_threads} threads)"
        );
        assert!(
            concurrent_peak.load(Ordering::SeqCst) > 1,
            "expected more than one segment to run concurrently"
        );
    }
}
