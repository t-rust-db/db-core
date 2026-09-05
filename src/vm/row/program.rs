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

use super::value::Collation;

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
    /// Jumps to the address stored (as an integer) in register `p1`.
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
    /// `r[p2] = r[p1]` verbatim.
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
}

/// One VDBE instruction: an opcode tag plus sqlite-rs's raw `p1..p5`
/// operands -- unchanged from sqlite-rs's shape (ADR 0008, revised),
/// so `codegen::row` (#20) can emit against this without any operand
/// reshaping.
#[derive(Debug, Clone, PartialEq)]
pub struct Instruction {
    pub opcode: Opcode,
    pub p1: i32,
    pub p2: i32,
    pub p3: i32,
    pub p4: P4,
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
    pub instructions: Vec<Instruction>,
}

impl Program {
    pub fn new(instructions: Vec<Instruction>) -> Self {
        Program { instructions }
    }

    pub fn push(&mut self, instr: Instruction) -> &mut Self {
        self.instructions.push(instr);
        self
    }
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
    fn instruction_with_comment_carries_it() {
        let instr = Instruction::new(Opcode::Halt, 0, 0, 0).with_comment("done");
        assert_eq!(instr.comment.as_deref(), Some("done"));
    }
}
