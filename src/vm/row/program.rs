//! Instruction format and the linear bytecode `Program`, ported from
//! sqlite-rs's `vdbe::program` (ADR 0008, revised): `Opcode` is a bare
//! tag enum and `Instruction` carries raw `p1..p5` operands exactly as
//! sqlite-rs does -- full opcode-identity parity, not a Rust-native
//! typed-operand redesign (that redesign was `vm::batch`-specific, per
//! ADR 0007, and does not apply here).
//!
//! **Every variant sqlite-rs's V2/V3 opcode set has is listed here**,
//! whether or not `vm::row`'s dispatch loop implements it yet -- this
//! enum is the single source of truth for "in scope for a full port",
//! matching sqlite-rs's own convention. See `super::vm`'s dispatch for
//! which opcodes are actually executable today (db-core#51's scope:
//! control flow, compare/cast/arithmetic, result-row loads except
//! `MakeRecord`, and `Rewind`/`Next`/`Column`/`Rowid` over the
//! storage-agnostic [`super::cursor::Cursor`] trait). Everything else
//! (DDL, sorter, hash aggregation, scalar functions, real transactions,
//! `MakeRecord`'s record encoding, the remaining cursor/index opcodes)
//! is unimplemented (`ExecError::Unimplemented`) pending later phases.

use crate::value::Collation;

/// `BEGIN`'s locking mode, carried through `Opcode::Transaction`'s `p1`
/// (db-core#97, mirroring sqlite-rs's `vdbe::control` constants).
pub const TRANSACTION_MODE_DEFERRED: i32 = 0;
/// `BEGIN IMMEDIATE`.
pub const TRANSACTION_MODE_IMMEDIATE: i32 = 1;
/// `BEGIN EXCLUSIVE`.
pub const TRANSACTION_MODE_EXCLUSIVE: i32 = 2;

/// `PRAGMA journal_mode`'s two supported values, carried through
/// `Opcode::SetJournalMode`'s `p1` (db-core#97).
pub const JOURNAL_MODE_DELETE: i32 = 0;
/// `PRAGMA journal_mode = WAL`.
pub const JOURNAL_MODE_WAL: i32 = 1;

/// `PRAGMA synchronous`'s supported levels, carried through
/// `Opcode::Synchronous`'s `p1` (db-core#97).
pub const SYNCHRONOUS_OFF: i32 = 0;
/// `PRAGMA synchronous = NORMAL`.
pub const SYNCHRONOUS_NORMAL: i32 = 1;
/// `PRAGMA synchronous = FULL`.
pub const SYNCHRONOUS_FULL: i32 = 2;
/// Sentinel `p1` for the bare `PRAGMA synchronous` query form (no
/// level to set, just report the current one).
pub const SYNCHRONOUS_QUERY: i32 = -1;

impl Opcode {
    /// The oracle-harvested opcode inventory sqlite-rs's
    /// `tests/unit/vdbe_opcode_completeness_test.rs` checks against
    /// `tools/opcodes-v2.json` — sqlite-rs `program.rs::Opcode::ALL`,
    /// verbatim (#134). Excludes sqlite-rs's own additions (`AutoCommit`,
    /// `SetJournalMode`, `Synchronous`, `IntegrityCheck`, the index-scan
    /// and DDL families), which the harvested set does not name.
    pub const ALL: [Opcode; 68] = [
        Opcode::Init,
        Opcode::Goto,
        Opcode::Once,
        Opcode::BeginSubrtn,
        Opcode::Return,
        Opcode::Halt,
        Opcode::Transaction,
        Opcode::IfNot,
        Opcode::IfNotZero,
        Opcode::IfPos,
        Opcode::DecrJumpZero,
        Opcode::IsNull,
        Opcode::NotNull,
        Opcode::MustBeInt,
        Opcode::OffsetLimit,
        Opcode::OpenRead,
        Opcode::OpenEphemeral,
        Opcode::OpenPseudo,
        Opcode::Rewind,
        Opcode::Last,
        Opcode::Next,
        Opcode::Column,
        Opcode::Rowid,
        Opcode::SeekRowid,
        Opcode::NullRow,
        Opcode::Sequence,
        Opcode::Found,
        Opcode::IdxInsert,
        Opcode::IdxLE,
        Opcode::Delete,
        Opcode::Eq,
        Opcode::Ge,
        Opcode::Gt,
        Opcode::Le,
        Opcode::Lt,
        Opcode::RealAffinity,
        Opcode::Add,
        Opcode::Subtract,
        Opcode::Multiply,
        Opcode::Divide,
        Opcode::Remainder,
        Opcode::Not,
        Opcode::BitAnd,
        Opcode::BitOr,
        Opcode::ShiftLeft,
        Opcode::ShiftRight,
        Opcode::BitNot,
        Opcode::Concat,
        Opcode::Cast,
        Opcode::Function,
        Opcode::AggStep,
        Opcode::AggFinal,
        Opcode::Copy,
        Opcode::Integer,
        Opcode::Int64,
        Opcode::Real,
        Opcode::Blob,
        Opcode::Null,
        Opcode::String8,
        Opcode::Variable,
        Opcode::MakeRecord,
        Opcode::ResultRow,
        Opcode::SorterOpen,
        Opcode::SorterInsert,
        Opcode::SorterSort,
        Opcode::SorterNext,
        Opcode::SorterData,
        Opcode::Sort,
    ];
}

/// sqlite-rs's VDBE opcode set, by category. See sqlite-rs's
/// `src/vdbe/program.rs` for the authoritative per-opcode semantics
/// this is ported from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Opcode {
    // control
    /// Jumps to `p2` (the program's real entry point) unless `p2` is 0.
    Init,
    /// Unconditional jump to `p2`.
    Goto,
    /// Runs its guarded body once per VM invocation; jumps to `p2` on
    /// every later visit to this same instruction.
    Once,
    /// Marks a subroutine's entry point; falls through.
    BeginSubrtn,
    /// Jumps to the address stored (as an integer) in register `p1`
    /// -- the address itself, as sqlite-rs does, not SQLite's
    /// `r[p1] + 1`; a future `Gosub` emitter stores the target it wants
    /// resumed at (#260). No codegen emits this yet.
    Return,
    /// Terminates execution. `p1` is the result code; `p4` may carry an
    /// error message.
    Halt,
    /// Begins a transaction. `p1` carries the transaction mode.
    Transaction,
    /// Explicit `COMMIT`/`ROLLBACK`. `p2`: 1 commits, 0 rolls back.
    AutoCommit,
    /// `PRAGMA journal_mode = ...`.
    SetJournalMode,
    /// `PRAGMA integrity_check`/`quick_check`.
    IntegrityCheck,
    /// `PRAGMA synchronous [= ...]`.
    Synchronous,
    /// Jumps to `p2` if register `p1` is falsy.
    IfNot,
    /// Jumps to `p2` if register `p1` is nonzero.
    IfNotZero,
    /// Jumps to `p2` if register `p1` is greater than zero.
    IfPos,
    /// Decrements register `p1`; jumps to `p2` if the result is zero.
    DecrJumpZero,
    /// Jumps to `p2` if register `p1` is NULL.
    IsNull,
    /// Jumps to `p2` if register `p1` is not NULL.
    NotNull,
    /// Coerces register `p1` to an integer, failing (or jumping to `p2`
    /// if nonzero) if it cannot be represented as one.
    MustBeInt,
    /// Computes LIMIT/OFFSET bookkeeping from registers `p1`
    /// (limit)/`p3` (offset) into register `p2`.
    OffsetLimit,
    // cursor
    /// Opens cursor `p1` for read-only access to the table/index with
    /// root page `p2`.
    OpenRead,
    /// Opens cursor `p1` for read/write access.
    OpenWrite,
    /// Opens cursor `p1` on a new, empty ephemeral b-tree.
    OpenEphemeral,
    /// Opens cursor `p1` as a second view onto ephemeral cursor `p2`.
    OpenDup,
    /// Opens cursor `p1` as a pseudo-cursor over one in-memory record.
    OpenPseudo,
    /// Positions cursor `p1` at its first entry, jumping to `p2` if
    /// empty.
    Rewind,
    /// Positions cursor `p1` at its last entry, jumping to `p2` if
    /// empty.
    Last,
    /// Advances cursor `p1`, jumping to `p2` if there was a next entry.
    Next,
    /// Reads column `p2` of cursor `p1`'s current row into register
    /// `p3`.
    Column,
    /// Stores cursor `p1`'s current rowid into register `p2`.
    Rowid,
    /// Seeks cursor `p1` to rowid `p3`, jumping to `p2` on miss.
    SeekRowid,
    /// Points cursor `p1` at a synthetic NULL row.
    NullRow,
    /// Stores cursor `p1`'s next sequence number into register `p2`.
    Sequence,
    /// Seeks cursor `p1` for a key from registers `p3..p3+p4`, jumping
    /// to `p2` if found.
    Found,
    /// Inserts the index entry in register `p2` into cursor `p1`.
    IdxInsert,
    /// Compares cursor `p1`'s key against `p3..p3+p4`, jumping to `p2`
    /// if `<=`.
    IdxLE,
    /// Deletes cursor `p1`'s current row.
    Delete,
    /// Inserts the record in register `p2` (rowid `p3`) into cursor
    /// `p1`.
    Insert,
    /// Generates a new rowid for cursor `p1`'s table into register
    /// `p2`.
    NewRowid,
    /// Deletes cursor `p1`'s current index entry.
    IdxDelete,
    /// Counts rows in the b-tree rooted at page `p1` into register
    /// `p2`.
    Count,
    /// Probes cursor `p1`'s index for a conflict from `p3..p3+p4`,
    /// jumping to `p2` if none.
    NoConflict,
    /// Probes cursor `p1`'s index for an exact key from `p3..p3+p4`,
    /// jumping to `p2` on miss.
    SeekIndexEq,
    /// Reads index cursor `p1`'s trailing rowid column into register
    /// `p2`.
    IdxRowid,
    /// Seeks index cursor `p1` to the first entry `>=` the key from
    /// `p3..p3+p4`, jumping to `p2` if none.
    SeekIndexGE,
    /// Compares index cursor `p1`'s entry against `p3..p3+p4`, jumping
    /// to `p2` if strictly greater.
    IdxCompareGT,
    /// Positions index cursor `p1` at its first entry, jumping to `p2`
    /// if empty.
    IdxRewind,
    /// Positions index cursor `p1` at its last entry, jumping to `p2`
    /// if empty.
    IdxLast,
    /// Advances index cursor `p1` forward, jumping to `p2` if there was
    /// a next entry.
    IdxNext,
    /// Advances index cursor `p1` backward, jumping to `p2` if there
    /// was a previous entry.
    IdxPrev,
    /// Appends rowid `p3` under the key from register `p2` into
    /// automatic-index cursor `p1`.
    AutoIndexInsert,
    /// Seeks automatic-index cursor `p1` by the key in register `p3`,
    /// jumping to `p2` if none match.
    AutoIndexSeek,
    /// Reads automatic-index cursor `p1`'s current rowid into register
    /// `p2`.
    AutoIndexRowid,
    /// Advances automatic-index cursor `p1`, jumping to `p2` if there
    /// was a next entry.
    AutoIndexNext,
    // DDL
    /// `CREATE TABLE`, per `p4`.
    CreateTable,
    /// `DROP TABLE`, per `p4`.
    DropTable,
    /// `CREATE INDEX`, per `p4`.
    CreateIndex,
    /// `DROP INDEX`, per `p4`.
    DropIndex,
    /// `CREATE VIEW`, per `p4`.
    CreateView,
    /// `ANALYZE`, per `p4`.
    Analyze,
    // compare (fused jump)
    /// Jumps to `p2` if registers `p1` and `p3` are equal, per `p4`'s
    /// collation/affinity.
    Eq,
    /// Jumps to `p2` if register `p3` is `>=` register `p1`.
    Ge,
    /// Jumps to `p2` if register `p3` is `>` register `p1`.
    Gt,
    /// Jumps to `p2` if register `p3` is `<=` register `p1`.
    Le,
    /// Jumps to `p2` if register `p3` is `<` register `p1`.
    Lt,
    /// Applies REAL affinity to register `p1` in place.
    RealAffinity,
    // arithmetic
    /// `r[p3] = r[p1] + r[p2]`.
    Add,
    /// `r[p3] = r[p2] - r[p1]` (sqlite-rs's operand order).
    Subtract,
    /// `r[p3] = r[p1] * r[p2]`.
    Multiply,
    /// `r[p3] = r[p2] / r[p1]`.
    Divide,
    /// `r[p3] = r[p2] % r[p1]`.
    Remainder,
    /// `r[p2] = !r[p1]`, three-valued.
    Not,
    /// `r[p3] = r[p1] & r[p2]`.
    BitAnd,
    /// `r[p3] = r[p1] | r[p2]`.
    BitOr,
    /// `r[p3] = r[p2] << r[p1]`.
    ShiftLeft,
    /// `r[p3] = r[p2] >> r[p1]`.
    ShiftRight,
    /// `r[p2] = ~r[p1]`.
    BitNot,
    /// `r[p3] = r[p2] || r[p1]`.
    Concat,
    /// Forces register `p1` to the affinity named by `p2`/`p4`, in
    /// place (`CAST`).
    Cast,
    // function
    /// Calls the scalar function named by `p4` with args
    /// `p2..p2+p5`, storing the result in `p3`.
    Function,
    // aggregate
    /// Feeds row `p2..p2+p5` into the aggregate accumulator in `p3`.
    AggStep,
    /// Finalizes the aggregate accumulator in `p1`.
    AggFinal,
    // result
    /// `r[p2] = p1` (small integer literal).
    Integer,
    /// `r[p2] = p4` (64-bit integer literal).
    Int64,
    /// `r[p2] = p4` (real literal).
    Real,
    /// `r[p2] = p4` (blob literal).
    Blob,
    /// Writes NULL into registers `p2..=max(p2,p3)`.
    Null,
    /// `r[p2] = p4` (text literal).
    String8,
    /// `r[p2] = ` bound parameter `p1` (1-based), or NULL if unbound.
    Variable,
    /// Serializes registers `p1..p1+p2` into a record blob into `p3`.
    MakeRecord,
    /// Emits registers `p1..p1+p2` as one output row.
    ResultRow,
    /// `r[p2..=p2+p3] = r[p1..=p1+p3]` verbatim -- `p3` extra registers
    /// beyond the first, SQLite's `OP_Copy` shape (#260); codegen emits
    /// `p3 = 0`.
    Copy,
    // sorter
    /// Opens a sorter on cursor `p1`, keyed per `p4`.
    SorterOpen,
    /// Inserts the record in register `p2` into sorter cursor `p1`.
    SorterInsert,
    /// Sorts sorter cursor `p1` and positions at the first record,
    /// jumping to `p2` if empty.
    SorterSort,
    /// Advances sorter cursor `p1`, jumping to `p2` if there was a next
    /// record.
    SorterNext,
    /// Stores sorter cursor `p1`'s current record into register `p2`.
    SorterData,
    /// Standalone in-place sort primitive.
    Sort,
    // hash aggregation
    /// Opens a hash-aggregation table on cursor `p1`, keyed per `p4`.
    HashAggOpen,
    /// Locates (creating on first sight) the group keyed by the record
    /// in register `p2`.
    HashAggFind,
    /// Folds `p2..p2+p5` into accumulator slot `p1` of cursor `p3`'s
    /// current group.
    HashAggStep,
    /// Freezes hash-aggregation cursor `p1`, orders its groups, and
    /// positions at the first, jumping to `p2` if none exist.
    HashAggRewind,
    /// Stores hash-aggregation cursor `p1`'s current group's row into
    /// register `p2`.
    HashAggData,
    /// Advances hash-aggregation cursor `p1`, jumping to `p2` if there
    /// was a next group.
    HashAggNext,
}

/// The dynamically-typed fourth operand, ported from sqlite-rs's
/// `vdbe::program::P4`. Only the variants `vm::row`'s current dispatch
/// scope needs are included; `SeekKey` (index-descriptor) is deferred
/// to the phase that implements index-scan opcodes, matching how
/// sqlite-rs itself grew `P4` incrementally rather than all at once.
#[derive(Debug, Clone, PartialEq)]
pub enum P4 {
    /// No P4 operand.
    None,
    /// An integer constant operand.
    Int(i64),
    /// A floating-point constant operand.
    Real(f64),
    /// A blob constant operand.
    Blob(Vec<u8>),
    /// A string constant, or function/index descriptor, operand.
    Str(String),
    /// A collation-sequence-plus-affinity descriptor for the compare
    /// opcodes.
    CollSeq {
        /// The collating sequence to compare under.
        collation: Collation,
        /// The comparison affinity byte, per SQLite's affinity codes.
        affinity: u8,
    },
    /// An affinity byte string, one byte per column, for `MakeRecord`.
    Affinity(Vec<u8>),
    /// Per-key-column collations for the index-key opcodes
    /// (`SeekIndexEq`/`GE`, `IdxCompareGT`, `IdxLE`, `Found`,
    /// `NoConflict`, `IdxInsert`, `AutoIndexInsert`/`Seek`); the key
    /// column count is the vector's length. `P4::Int(n)` on the same
    /// opcodes means `n` BINARY-collated columns (#134).
    SeekKey(Vec<Collation>),
    /// A boolean flag operand (#134).
    Bool(bool),
    /// `AggStep`'s `"name(arity)"` descriptor plus the collation
    /// `min`/`max` compares under -- `AggFinal` has no comparison to
    /// perform, so it keeps the plain `Str` descriptor.
    AggFunc {
        /// The aggregate function's name.
        name: String,
        /// The aggregate function's argument count.
        arity: usize,
        /// The collation `min`/`max` compares under.
        collation: Collation,
    },
    /// `SorterOpen`'s sort-key descriptor (db-core#69, extended to
    /// multi-key by db-core#87): one entry per `ORDER BY` term, applied
    /// in order (matching sqlite-rs's `P4::SortKey`).
    SortKey(Vec<SortKeyColumn>),
    /// `HashAggOpen`'s group-key descriptor (db-core#86): one entry per
    /// `GROUP BY` term, in `GROUP BY` order.
    GroupKey(Vec<GroupKeyColumn>),
    /// `CreateTable` (db-core#97): the new table's name and verbatim
    /// `sqlite_master.sql` text.
    CreateTable {
        /// The new table's name.
        name: String,
        /// The verbatim `sqlite_master.sql` text.
        sql: String,
    },
    /// `CreateView` (db-core#97): same shape as `CreateTable`'s payload,
    /// its own variant so a `Program`'s P4 operand names the DDL kind
    /// it actually came from.
    CreateView {
        /// The new view's name.
        name: String,
        /// The verbatim `sqlite_master.sql` text.
        sql: String,
    },
    /// `DropTable` (db-core#97): the target table's name/root page,
    /// plus every index on it (`(name, root_page)`) to cascade-drop.
    DropTable {
        /// The target table's name.
        name: String,
        /// The target table's root page.
        root_page: u32,
        /// Every index on the table, as `(name, root_page)`.
        indexes: Vec<(String, u32)>,
    },
    /// `CreateIndex` (db-core#97): the new index's name, its target
    /// table's name/root page, verbatim `sqlite_master.sql` text, the
    /// indexed columns' 0-based positions, and the `UNIQUE` flag.
    CreateIndex {
        /// The new index's name.
        name: String,
        /// The target table's name.
        table_name: String,
        /// The target table's root page.
        table_root_page: u32,
        /// The verbatim `sqlite_master.sql` text.
        sql: String,
        /// The indexed columns' 0-based positions in table-column order.
        column_indices: Vec<usize>,
        /// Whether the index enforces a `UNIQUE` constraint.
        unique: bool,
    },
    /// `DropIndex` (db-core#97): the target index's name/root page.
    DropIndex {
        /// The target index's name.
        name: String,
        /// The target index's root page.
        root_page: u32,
    },
    /// `Analyze` (db-core#97): every table `ANALYZE` should populate
    /// stats for -- baked at codegen time from the schema catalog.
    Analyze {
        /// Every table (and its indexes) `ANALYZE` should populate
        /// stats for.
        targets: Vec<AnalyzeTarget>,
    },
}

/// One table `ANALYZE` (db-core#97) populates `sqlite_stat1` for: its
/// name and table-b-tree root page, plus every index on it (name + root
/// page) to walk for index-level stats.
#[derive(Debug, Clone, PartialEq)]
pub struct AnalyzeTarget {
    /// The table's name.
    pub table_name: String,
    /// The table b-tree's root page.
    pub table_root_page: u32,
    /// The table's indexes, each walked for index-level stats.
    pub indexes: Vec<AnalyzeIndexTarget>,
}

/// One index `ANALYZE` (db-core#97) walks to compute `avg_eq` for.
#[derive(Debug, Clone, PartialEq)]
pub struct AnalyzeIndexTarget {
    /// The index's name.
    pub index_name: String,
    /// The index b-tree's root page.
    pub root_page: u32,
}

/// One `ORDER BY` sort key: which record column to compare, its
/// direction, collation, and NULL placement. Ported from sqlite-rs's
/// `SortKeyColumn`; `P4::SortKey` carries one of these per key column
/// (db-core#69 landed the single-key case, db-core#87 the `Vec`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortKeyColumn {
    /// The record column index this key compares.
    pub index: usize,
    /// `true` for `DESC`.
    pub descending: bool,
    /// The collation to compare text under.
    pub collation: Collation,
    /// Where NULLs sort: `true` for `NULLS FIRST`, `false` for `NULLS
    /// LAST`.
    pub nulls_first: bool,
}

/// One `GROUP BY` key column: which record column to group on, and the
/// collation two values must compare equal under to land in the same
/// group (post `apply_affinity`/comparison-affinity rules, same as the
/// sort strategy's group-boundary `Eq`). Ported from sqlite-rs's
/// `GroupKeyColumn` (db-core#86).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupKeyColumn {
    /// The record column index this key groups on.
    pub index: usize,
    /// The collation two values must compare equal under to be the
    /// same group.
    pub collation: Collation,
    /// The comparison affinity applied to the key value before hashing
    /// (`Affinity::to_p4_byte`), so `1` and `'1'` land in one group under
    /// NUMERIC affinity exactly as the sort strategy's group-boundary
    /// `Eq` would judge them (sqlite-rs `hash_agg::group_key_into`, #134).
    pub affinity: u8,
}

/// One VDBE instruction: an opcode tag plus sqlite-rs's raw `p1..p5`
/// operands -- unchanged from sqlite-rs's shape (ADR 0008, revised),
/// so `codegen::row` (#20) can emit against this without any operand
/// reshaping.
#[derive(Debug, Clone, PartialEq)]
pub struct Instruction {
    /// The instruction's opcode tag.
    pub opcode: Opcode,
    /// First integer operand.
    pub p1: i32,
    /// Second integer operand.
    pub p2: i32,
    /// Third integer operand.
    pub p3: i32,
    /// Dynamically-typed fourth operand.
    pub p4: P4,
    /// Flags operand.
    pub p5: u16,
    /// Optional `EXPLAIN` comment (ADR 0007's convention, kept for
    /// `vm::row` too).
    pub comment: Option<String>,
}

impl Instruction {
    /// Builds an instruction with `p4` absent and `p5` zero.
    pub fn new(opcode: Opcode, p1: i32, p2: i32, p3: i32) -> Self {
        Instruction {
            opcode,
            p1,
            p2,
            p3,
            p4: P4::None,
            p5: 0,
            comment: None,
        }
    }

    /// Builds an instruction carrying a `p4` operand.
    pub fn with_p4(opcode: Opcode, p1: i32, p2: i32, p3: i32, p4: P4) -> Self {
        Instruction {
            opcode,
            p1,
            p2,
            p3,
            p4,
            p5: 0,
            comment: None,
        }
    }

    /// Attaches an `EXPLAIN` comment to this instruction.
    pub fn with_comment(mut self, comment: impl Into<String>) -> Self {
        self.comment = Some(comment.into());
        self
    }
}

/// A linear, zero-indexed instruction sequence. Execution starts at PC
/// 0 and advances by incrementing PC unless an instruction explicitly
/// redirects it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Program {
    /// The program's instructions, in execution order.
    pub instructions: Vec<Instruction>,
    /// Slot-indexed (0 = bind-parameter slot 1) names for every
    /// `Opcode::Variable` slot this program's compile allocated --
    /// `None` for an anonymous (`?`)/numbered (`?NNN`) slot, `Some` for
    /// a named one (`:name`/`@name`/`$name`) (db-core#162). Empty when
    /// the compiler that produced this program never wired it up (e.g.
    /// a statement kind with no bind-parameter support yet); positional
    /// binding via [`crate::vm::row::Vm::bind_params`] does not depend
    /// on this being populated.
    pub param_names: Vec<Option<String>>,
}

impl Program {
    /// Builds a program from its instruction sequence.
    pub fn new(instructions: Vec<Instruction>) -> Self {
        Program {
            instructions,
            param_names: Vec::new(),
        }
    }

    /// Attaches bind-parameter slot names, for a caller that wants to
    /// bind `:name`/`@name`/`$name` forms by name.
    #[must_use]
    pub fn with_param_names(mut self, param_names: Vec<Option<String>>) -> Self {
        self.param_names = param_names;
        self
    }

    /// Appends `instr` to the end of the program, returning `self` for chaining.
    pub fn push(&mut self, instr: Instruction) -> &mut Self {
        self.instructions.push(instr);
        self
    }

    /// The instruction at `pc`, if any.
    pub fn get(&self, pc: usize) -> Option<&Instruction> {
        self.instructions.get(pc)
    }

    /// Number of instructions.
    pub fn len(&self) -> usize {
        self.instructions.len()
    }

    /// `true` for a program with no instructions.
    pub fn is_empty(&self) -> bool {
        self.instructions.is_empty()
    }
}

/// Which of an instruction's `p1`/`p2`/`p3` name a register (as opposed
/// to a cursor slot, jump target, literal, count, root page, affinity
/// byte or flag). Exhaustive over [`Opcode`] so a new variant cannot
/// land unclassified (#257).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegisterOperands {
    /// `p1` is a register.
    pub p1: bool,
    /// `p2` is a register.
    pub p2: bool,
    /// `p3` is a register.
    pub p3: bool,
}

const NONE: RegisterOperands = RegisterOperands {
    p1: false,
    p2: false,
    p3: false,
};
const P1: RegisterOperands = RegisterOperands {
    p1: true,
    p2: false,
    p3: false,
};
const P2: RegisterOperands = RegisterOperands {
    p1: false,
    p2: true,
    p3: false,
};
const P3: RegisterOperands = RegisterOperands {
    p1: false,
    p2: false,
    p3: true,
};
const P1_P2: RegisterOperands = RegisterOperands {
    p1: true,
    p2: true,
    p3: false,
};
const P1_P3: RegisterOperands = RegisterOperands {
    p1: true,
    p2: false,
    p3: true,
};
const P2_P3: RegisterOperands = RegisterOperands {
    p1: false,
    p2: true,
    p3: true,
};
const P1_P2_P3: RegisterOperands = RegisterOperands {
    p1: true,
    p2: true,
    p3: true,
};

impl Opcode {
    /// Which integer operands of this opcode are register numbers, per
    /// `vm::row`'s dispatch (#257). A range's *first* register counts
    /// (`ResultRow`'s `p1`, `Function`'s `p2`); the range length lives in
    /// a count operand or `P4` and is not a register.
    pub fn register_operands(self) -> RegisterOperands {
        match self {
            // Control: jump targets, flags, modes -- no registers.
            Opcode::Init
            | Opcode::Goto
            | Opcode::Once
            | Opcode::BeginSubrtn
            | Opcode::Halt
            | Opcode::Transaction
            | Opcode::AutoCommit
            | Opcode::SetJournalMode
            | Opcode::IntegrityCheck
            | Opcode::Synchronous => NONE,
            Opcode::Return
            | Opcode::IfNot
            | Opcode::IfNotZero
            | Opcode::IfPos
            | Opcode::DecrJumpZero
            | Opcode::IsNull
            | Opcode::NotNull
            | Opcode::MustBeInt
            | Opcode::RealAffinity
            | Opcode::Cast => P1,
            Opcode::OffsetLimit => P1_P2_P3,

            // Cursors: `p1` is the slot; `p2` a root page / jump target /
            // source slot unless noted.
            Opcode::OpenRead
            | Opcode::OpenWrite
            | Opcode::OpenEphemeral
            | Opcode::OpenDup
            | Opcode::Rewind
            | Opcode::Last
            | Opcode::Next
            | Opcode::NullRow
            | Opcode::Delete
            | Opcode::IdxRewind
            | Opcode::IdxLast
            | Opcode::IdxNext
            | Opcode::IdxPrev
            | Opcode::AutoIndexNext
            | Opcode::SorterSort
            | Opcode::SorterNext
            | Opcode::Sort
            | Opcode::HashAggOpen
            | Opcode::HashAggRewind
            | Opcode::HashAggNext
            | Opcode::CreateTable
            | Opcode::DropTable
            | Opcode::CreateIndex
            | Opcode::DropIndex
            | Opcode::CreateView
            | Opcode::Analyze => NONE,
            Opcode::OpenPseudo
            | Opcode::Rowid
            | Opcode::Sequence
            | Opcode::NewRowid
            | Opcode::Count
            | Opcode::IdxRowid
            | Opcode::AutoIndexRowid
            | Opcode::IdxInsert
            | Opcode::IdxDelete
            | Opcode::SorterInsert
            | Opcode::SorterData
            | Opcode::HashAggFind
            | Opcode::HashAggData => P2,
            // `SorterOpen`'s `p2` is the LIMIT register when `p5 != 0`;
            // treating it as a register when `p5 == 0` (codegen emits 0
            // there) over-reserves nothing.
            Opcode::SorterOpen => P2,
            Opcode::Column
            | Opcode::SeekRowid
            | Opcode::Found
            | Opcode::IdxLE
            | Opcode::NoConflict
            | Opcode::SeekIndexEq
            | Opcode::SeekIndexGE
            | Opcode::IdxCompareGT
            | Opcode::AutoIndexSeek
            | Opcode::AggFinal => P3,
            Opcode::Insert | Opcode::AutoIndexInsert => P2_P3,

            // Compare: `r[p1] <op> r[p3]`, jump `p2`.
            Opcode::Eq | Opcode::Ge | Opcode::Gt | Opcode::Le | Opcode::Lt => P1_P3,

            // Arithmetic: two sources, one destination.
            Opcode::Add
            | Opcode::Subtract
            | Opcode::Multiply
            | Opcode::Divide
            | Opcode::Remainder
            | Opcode::BitAnd
            | Opcode::BitOr
            | Opcode::ShiftLeft
            | Opcode::ShiftRight
            | Opcode::Concat => P1_P2_P3,
            Opcode::Not | Opcode::BitNot | Opcode::Copy => P1_P2,

            // Functions/aggregates: `p2` is the first argument register;
            // `AggStep`'s `p1` is an aggregate slot, `HashAggStep`'s `p3` a
            // cursor slot.
            Opcode::Function => P2_P3,
            Opcode::AggStep | Opcode::HashAggStep => P2,

            // Loads: `p1` is the literal for `Integer`, unused otherwise.
            Opcode::Integer
            | Opcode::Int64
            | Opcode::Real
            | Opcode::Blob
            | Opcode::String8
            | Opcode::Variable => P2,
            Opcode::Null => P2_P3,
            // `p1..p1+p2` source range, `p3` destination / `p2` count.
            Opcode::MakeRecord => P1_P3,
            Opcode::ResultRow => P1,
        }
    }
}

impl Instruction {
    /// The highest register this instruction statically names, `None`
    /// if it names no register (#257). Counted ranges whose length is an
    /// operand (`ResultRow`/`MakeRecord`'s `p1..p1+p2`, `Null`'s
    /// `p2..=p3`) are included; ranges whose length lives in `P4`
    /// (`Function`, `AggStep`, the index-key opcodes) contribute only
    /// their first register -- the VM's lazy register growth covers the
    /// rest.
    pub fn max_register(&self) -> Option<i32> {
        let ops = self.opcode.register_operands();
        let mut max: Option<i32> = None;
        let mut consider = |reg: i32| {
            max = Some(max.map_or(reg, |m| m.max(reg)));
        };
        if ops.p1 {
            consider(self.p1);
        }
        if ops.p2 {
            consider(self.p2);
        }
        if ops.p3 {
            consider(self.p3);
        }
        if matches!(self.opcode, Opcode::ResultRow | Opcode::MakeRecord) {
            // `p1..p1+p2`: the last register is `p1 + p2 - 1`; a
            // non-positive count names only `p1`.
            if let Some(last) = self
                .p2
                .checked_sub(1)
                .filter(|n| *n >= 0)
                .and_then(|n| self.p1.checked_add(n))
            {
                consider(last);
            }
        }
        max
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_push_appends_instructions_in_order() {
        let mut program = Program::default();
        program
            .push(Instruction::new(Opcode::Integer, 1, 0, 0))
            .push(Instruction::new(Opcode::Halt, 0, 0, 0));
        assert_eq!(program.instructions.len(), 2);
        assert_eq!(program.instructions[1].opcode, Opcode::Halt);
    }

    #[test]
    fn max_register_ignores_literal_and_slot_operands() {
        // `Integer`'s p1 is the value, not a register (#257).
        assert_eq!(
            Instruction::new(Opcode::Integer, 1_000_000, 3, 0).max_register(),
            Some(3)
        );
        // `Column`: p1 cursor slot, p2 column index, p3 register.
        assert_eq!(
            Instruction::new(Opcode::Column, 0, 99, 4).max_register(),
            Some(4)
        );
        // `OpenRead`: root page in p2 is not a register.
        assert_eq!(
            Instruction::new(Opcode::OpenRead, 0, 5000, 0).max_register(),
            None
        );
        // Jumps name no registers; compares name p1/p3 but not the target.
        assert_eq!(
            Instruction::new(Opcode::Goto, 0, 500, 0).max_register(),
            None
        );
        assert_eq!(
            Instruction::new(Opcode::Eq, 2, 500, 7).max_register(),
            Some(7)
        );
        // Counted ranges reach their last register.
        assert_eq!(
            Instruction::new(Opcode::ResultRow, 10, 3, 0).max_register(),
            Some(12)
        );
        assert_eq!(
            Instruction::new(Opcode::MakeRecord, 10, 3, 2).max_register(),
            Some(12)
        );
        assert_eq!(
            Instruction::new(Opcode::ResultRow, 10, 0, 0).max_register(),
            Some(10)
        );
        assert_eq!(
            Instruction::new(Opcode::Null, 0, 4, 9).max_register(),
            Some(9)
        );
    }

    #[test]
    fn instruction_with_comment_carries_it() {
        let instr = Instruction::new(Opcode::Halt, 0, 0, 0).with_comment("done");
        assert_eq!(instr.comment.as_deref(), Some("done"));
    }
}
