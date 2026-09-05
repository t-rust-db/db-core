//! The register file, cursor-slot table, aggregate-context slot table,
//! and fetch-decode-execute loop, ported from sqlite-rs's `vdbe::exec`
//! (db-core#51/#56/#59/#62/#64/#68/#69). Storage-agnostic: cursors are
//! [`super::cursor::Cursor`] trait objects, not real b-tree/pager
//! access -- see `super::cursor`'s doc and ADR 0008.
//!
//! **Scope of this phase.** Dispatched: control flow (`Init`/`Goto`/
//! `Once`/`BeginSubrtn`/`Return`/`Halt`/`IfNot`/`IfNotZero`/`IfPos`/
//! `DecrJumpZero`/`IsNull`/`NotNull`/`MustBeInt`/`OffsetLimit`), the
//! fused compare-jump opcodes (`Eq`/`Ge`/`Gt`/`Le`/`Lt`), `RealAffinity`/
//! `Cast`, arithmetic (`Add`/`Subtract`/`Multiply`/`Divide`/`Remainder`/
//! `Not`/`BitAnd`/`BitOr`/`ShiftLeft`/`ShiftRight`/`BitNot`/`Concat`),
//! result-row loads (`Integer`/`Int64`/`Real`/`Blob`/`Null`/`String8`/
//! `Variable`/`Copy`/`ResultRow`/`MakeRecord`, via [`super::record`]),
//! `Rewind`/`Next`/`Column`/`Rowid` over a [`super::cursor::Cursor`]
//! opened via [`Vm::open_cursor`], `OpenEphemeral`/`Insert` over an
//! in-memory [`super::cursor::EphemeralTableCursor`] (`Insert` decodes
//! `MakeRecord`'s blob straight back into `Value`s -- sqlite-rs's own
//! "decode-once-at-insert" design), `AggStep`/`AggFinal` over
//! [`super::aggregate::AggState`] (single-group only), `Function` over
//! [`super::functions::call`], and `SorterOpen`/`SorterInsert`/
//! `SorterSort`/`Sort`/`SorterNext`/`SorterData` over a
//! [`super::cursor::SorterCursor`] (single-key, no LIMIT/bound).
//! Everything else in [`super::program::Opcode`] returns
//! `ExecError::Unimplemented`.

use std::cmp::Ordering;
use std::collections::HashSet;

use super::affinity::{apply_affinity, Affinity};
use super::aggregate::{self, AggState};
use super::cast::cast_to;
use super::coerce;
use super::compare::compare;
use super::cursor::{Cursor, EphemeralTableCursor, HashAggCursor, PseudoCursor, SorterCursor};
use super::cursor_factory::{CursorFactory, CursorFactoryError};
use super::functions;
use super::program::{Instruction, Opcode, Program, P4, SYNCHRONOUS_FULL, SYNCHRONOUS_QUERY};
use super::record::{decode_column, decode_record, encode_record};
use super::schema_storage::{SchemaStorage, SchemaStorageError};
use super::transaction::Transaction;
use super::value::{Collation, TextEncoding, Value};

/// Caps a single register index or range count -- a backstop against an
/// adversarial/corrupt instruction driving an oversized allocation.
pub(crate) const MAX_REGISTERS: usize = 1 << 20;

/// A backstop against a runaway/looping program with no `Halt`.
pub const MAX_STEPS: u64 = 1 << 24;

/// The ways the fetch-decode-execute loop can fail to run a [`Program`]
/// to completion.
#[derive(Debug)]
pub enum ExecError {
    /// `opcode` addressed register `index`, which lies outside the
    /// register file.
    RegisterOutOfRange { opcode: &'static str, index: i32 },
    /// `opcode` requested a register range of `count` registers, more
    /// than [`MAX_REGISTERS`] allows.
    RegisterRangeTooLarge { opcode: &'static str, count: i32 },
    /// `opcode` required a register to hold a particular [`Value`]
    /// variant but found `found` instead.
    TypeMismatch {
        opcode: &'static str,
        found: &'static str,
    },
    /// `MustBeInt`'s coercion failed.
    MustBeInt,
    /// `opcode`'s operands are structurally invalid for `reason`.
    MalformedInstruction {
        opcode: &'static str,
        reason: String,
    },
    /// `opcode` is a recognized opcode with no dispatch arm yet.
    Unimplemented { opcode: Opcode },
    /// `slot` was referenced but has no cursor open in it.
    CursorNotOpen { slot: i32 },
    /// A jump or fall-through moved the program counter past the end of
    /// the program's instructions.
    ProgramCounterOutOfRange { pc: usize },
    /// The program executed [`MAX_STEPS`] instructions without halting.
    StepLimitExceeded,
    /// The program executed `Halt` with a non-success result `code`.
    Halted { code: i32, message: Option<String> },
    /// `SetJournalMode` ran while a transaction was open
    /// (`!Vm::autocommit`) -- stock SQLite refuses to change journal
    /// mode mid-transaction.
    JournalModeChangeDuringTransaction,
    /// `Transaction` while one is already open (sqlite-rs, #134).
    TransactionAlreadyActive,
    /// `AutoCommit` commit with no open transaction (#134).
    NoActiveTransactionToCommit,
    /// `AutoCommit` rollback with no open transaction (#134).
    NoActiveTransactionToRollback,
    /// A [`super::transaction::Transaction`] hook's `begin`/`commit`/
    /// `rollback` failed (`Opcode::Transaction`/`AutoCommit`,
    /// db-core#81).
    TransactionFailed(super::transaction::TransactionError),
    /// `OpenRead`/`OpenWrite` named a nonzero `p3` (attached-database
    /// index) -- db-core has no notion of attached databases
    /// (db-core#125).
    AttachedDatabasesUnsupported,
    /// A [`super::cursor_factory::CursorFactory`] call failed
    /// (db-core#125).
    CursorFactoryFailed(CursorFactoryError),
    /// `opcode` needs a [`super::schema_storage::SchemaStorage`] hook
    /// but none is installed (db-core#128).
    SchemaStorageMissing { opcode: &'static str },
    /// A [`super::schema_storage::SchemaStorage`] call failed
    /// (db-core#128).
    SchemaStorageFailed(SchemaStorageError),
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecError::RegisterOutOfRange { opcode, index } => {
                write!(f, "{opcode}: register index {index} is out of range")
            }
            ExecError::RegisterRangeTooLarge { opcode, count } => write!(
                f,
                "{opcode}: register range count {count} exceeds the maximum ({MAX_REGISTERS})"
            ),
            ExecError::TypeMismatch { opcode, found } => {
                write!(
                    f,
                    "{opcode}: expected a different value type, found {found}"
                )
            }
            ExecError::MustBeInt => write!(
                f,
                "MustBeInt: value cannot be converted to an integer without data loss"
            ),
            ExecError::MalformedInstruction { opcode, reason } => {
                write!(f, "{opcode}: malformed instruction ({reason})")
            }
            ExecError::Unimplemented { opcode } => {
                write!(f, "opcode {opcode:?} is not yet implemented by this VM")
            }
            ExecError::CursorNotOpen { slot } => write!(f, "cursor slot {slot} is not open"),
            ExecError::ProgramCounterOutOfRange { pc } => {
                write!(f, "program counter {pc} is out of range")
            }
            ExecError::StepLimitExceeded => write!(
                f,
                "program exceeded the maximum step count ({MAX_STEPS}) without halting"
            ),
            ExecError::Halted { code, message } => write!(
                f,
                "statement halted with result code {code}{}",
                message
                    .as_deref()
                    .map(|m| format!(": {m}"))
                    .unwrap_or_default()
            ),
            ExecError::JournalModeChangeDuringTransaction => write!(
                f,
                "SetJournalMode: cannot change journal mode within a transaction"
            ),
            ExecError::TransactionAlreadyActive => {
                write!(f, "cannot start a transaction within a transaction")
            }
            ExecError::NoActiveTransactionToCommit => {
                write!(f, "cannot commit - no transaction is active")
            }
            ExecError::NoActiveTransactionToRollback => {
                write!(f, "cannot rollback - no transaction is active")
            }
            ExecError::TransactionFailed(err) => write!(f, "transaction hook failed: {err}"),
            ExecError::AttachedDatabasesUnsupported => {
                write!(f, "attached databases are not supported")
            }
            ExecError::CursorFactoryFailed(err) => write!(f, "cursor factory failed: {err}"),
            ExecError::SchemaStorageMissing { opcode } => {
                write!(f, "{opcode}: no schema storage hook is installed")
            }
            ExecError::SchemaStorageFailed(err) => write!(f, "schema storage failed: {err}"),
        }
    }
}

impl std::error::Error for ExecError {}

/// The outcome of executing one instruction: fall through to PC+1, jump
/// to an explicit target, or halt the program.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Next,
    Jump(usize),
    Halt { code: i32, message: Option<String> },
}

#[allow(clippy::cast_sign_loss)]
fn to_pc(p2: i32) -> usize {
    p2.max(0) as usize
}

fn value_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "NULL",
        Value::Integer(_) => "INTEGER",
        Value::Real(_) => "REAL",
        Value::Text(_) => "TEXT",
        Value::Blob(_) => "BLOB",
    }
}

/// Truthiness for the boolean-consuming opcodes: numeric-coerced zero
/// is false, everything else is true. NULL answers `false` here (it is
/// neither true nor false; callers decide what NULL means for them).
pub(crate) fn is_falsy(v: &Value) -> bool {
    match v {
        Value::Integer(i) => *i == 0,
        Value::Real(r) => *r == 0.0,
        Value::Null => false,
        Value::Text(s) => match coerce::coerce_text_to_numeric(s) {
            Value::Integer(i) => i == 0,
            Value::Real(r) => r == 0.0,
            _ => true,
        },
        Value::Blob(_) => true,
    }
}

/// The VM's mutable execution state: a register file of `Value` cells,
/// a disjoint cursor-slot table, a disjoint aggregate-context slot
/// table (`AggStep`/`AggFinal`'s `p1`, same shape as `cursors`), plus
/// accumulated output rows and `Once`'s one-shot-guard bookkeeping.
pub struct Vm {
    registers: Vec<Value>,
    cursors: Vec<Option<Box<dyn Cursor>>>,
    agg_contexts: Vec<Option<AggState>>,
    rows: Vec<Vec<Value>>,
    once_fired: HashSet<usize>,
    params: Vec<Value>,
    /// Whether this `Vm` is outside an explicit transaction (db-core#89)
    /// -- `true` until `Opcode::Transaction` clears it, restored by
    /// `Opcode::AutoCommit`. `Opcode::SetJournalMode` consults it
    /// (matches stock SQLite's refusal to change journal mode
    /// mid-transaction); with no [`Self::transaction_hook`] installed,
    /// this flag is the only real effect `Transaction`/`AutoCommit`
    /// have, since `db-core` has no pager of its own.
    autocommit: bool,
    /// A consumer's pager, installed via
    /// [`Self::set_transaction_hook`], observing/driving real
    /// transaction semantics when `Opcode::Transaction`/`AutoCommit`
    /// run (db-core#81). `None` (the default) means those opcodes only
    /// toggle [`Self::autocommit`].
    transaction_hook: Option<Box<dyn Transaction>>,
    /// A consumer's cursor-opening hook, installed via
    /// [`Self::set_cursor_factory`] (db-core#125). `None` (the default)
    /// means `OpenRead`/`OpenWrite` fall back to the pre-wired path:
    /// asserting the caller already opened the slot via
    /// [`Self::open_cursor`] before running the program.
    cursor_factory: Option<Box<dyn CursorFactory>>,
    /// The root page each open cursor slot was last opened against,
    /// parallel to `cursors` -- `OpenDup`'s only way to re-derive which
    /// root to hand the factory for a second cursor onto the same
    /// table (db-core#125).
    cursor_roots: Vec<Option<u32>>,
    /// Per-slot register an `OpenPseudo` cursor reads its row from, lazily
    /// at `Column` time (sqlite-rs `CursorSlot::Pseudo`, #134).
    pseudo_regs: Vec<Option<i32>>,
    /// A consumer's schema-write hook, installed via
    /// [`Self::set_schema_storage`] (db-core#128). `None` (the
    /// default) means `CreateTable`/`CreateIndex`/`DropTable`/
    /// `DropIndex`/`Analyze` fail with
    /// [`ExecError::SchemaStorageMissing`].
    schema_storage: Option<Box<dyn SchemaStorage>>,
    /// Which open cursor slots `Opcode::NullRow` has pointed at a
    /// synthetic all-NULL row (db-core#127, `LEFT JOIN`'s unmatched
    /// side) -- parallel to `cursors`. Cleared by any repositioning
    /// opcode (`Rewind`/`Next`/`Last`/`Prev`/`SeekRowid`) on that slot.
    null_rows: Vec<bool>,
    /// Per-cursor-slot monotonic counters for `Opcode::Sequence`
    /// (db-core#127) -- parallel to `cursors`, though `Sequence` never
    /// requires the slot to actually hold an open cursor (it's purely a
    /// counter identified by `p1`).
    sequences: Vec<Option<i64>>,
    /// The database's text encoding, for decoding `Insert` payloads into
    /// in-memory cursors; `Utf8` unless a consumer sets it (#134).
    text_encoding: TextEncoding,
}

impl Default for Vm {
    fn default() -> Self {
        Vm {
            registers: Vec::new(),
            cursors: Vec::new(),
            agg_contexts: Vec::new(),
            rows: Vec::new(),
            once_fired: HashSet::new(),
            params: Vec::new(),
            autocommit: true,
            transaction_hook: None,
            cursor_factory: None,
            cursor_roots: Vec::new(),
            pseudo_regs: Vec::new(),
            schema_storage: None,
            null_rows: Vec::new(),
            sequences: Vec::new(),
            text_encoding: TextEncoding::Utf8,
        }
    }
}

impl Vm {
    pub fn new() -> Self {
        Vm::default()
    }

    /// Binds parameter values for `Opcode::Variable`, 1-based.
    pub fn bind_params(&mut self, values: Vec<Value>) {
        self.params = values;
    }

    /// Installs `hook` to observe/drive `Opcode::Transaction`/
    /// `AutoCommit` (db-core#81). With none installed (the default),
    /// those opcodes only toggle [`Vm::autocommit`].
    pub fn set_transaction_hook(&mut self, hook: Box<dyn Transaction>) {
        self.transaction_hook = Some(hook);
    }

    /// Installs `factory` to resolve `OpenRead`/`OpenWrite`'s `p2` root
    /// page to a real cursor (db-core#125). With none installed (the
    /// default), those opcodes fall back to asserting the slot was
    /// pre-wired via [`Self::open_cursor`].
    pub fn set_cursor_factory(&mut self, factory: Box<dyn CursorFactory>) {
        self.cursor_factory = Some(factory);
    }

    /// Installs `storage` to back `CreateTable`/`CreateIndex`/
    /// `DropTable`/`DropIndex`/`Analyze` (db-core#128). With none
    /// installed (the default), those opcodes fail with
    /// [`ExecError::SchemaStorageMissing`].
    /// Whether the VM is outside an explicit transaction. A consumer
    /// driving a multi-statement session (sqlite-rs's REPL/`exec`) carries
    /// this across programs: read it after `execute`, seed the next `Vm`
    /// with [`Self::set_autocommit`].
    pub fn autocommit(&self) -> bool {
        self.autocommit
    }

    /// Seeds the autocommit state (see [`Self::autocommit`]); `false`
    /// means "a `BEGIN` from an earlier program is still open".
    pub fn set_autocommit(&mut self, autocommit: bool) {
        self.autocommit = autocommit;
    }

    /// The database's text encoding (header byte 56); decoded `Insert`
    /// payloads use it. Default `Utf8` (#134).
    pub fn set_text_encoding(&mut self, encoding: TextEncoding) {
        self.text_encoding = encoding;
    }

    pub fn set_schema_storage(&mut self, storage: Box<dyn SchemaStorage>) {
        self.schema_storage = Some(storage);
    }

    /// Marks cursor slot `slot` as pointed at a synthetic NULL row
    /// (`Opcode::NullRow`, db-core#127).
    fn set_null_row(&mut self, slot: i32) -> Result<(), ExecError> {
        let idx = Self::index("cursor slot write", slot)?;
        if idx >= self.null_rows.len() {
            self.null_rows.resize(idx.saturating_add(1), false);
        }
        if let Some(cell) = self.null_rows.get_mut(idx) {
            *cell = true;
        }
        Ok(())
    }

    /// Clears slot `slot`'s NULL-row flag -- called by every opcode
    /// that repositions a cursor, so a later normal read doesn't keep
    /// reading NULLs.
    fn clear_null_row(&mut self, slot: i32) -> Result<(), ExecError> {
        let idx = Self::index("cursor slot write", slot)?;
        if let Some(cell) = self.null_rows.get_mut(idx) {
            *cell = false;
        }
        Ok(())
    }

    fn is_null_row(&self, slot: i32) -> Result<bool, ExecError> {
        let idx = Self::index("cursor slot read", slot)?;
        Ok(self.null_rows.get(idx).copied().unwrap_or(false))
    }

    /// Reads slot `slot`'s next `Opcode::Sequence` value and advances
    /// it (db-core#127) -- growing the counter table with zero filler
    /// as needed, same policy as `open_cursor`/`set_agg_context`.
    fn next_sequence(&mut self, slot: i32) -> Result<i64, ExecError> {
        let idx = Self::index("sequence slot", slot)?;
        // Seeded from the cursor kind: 1 for an ephemeral table, 0
        // otherwise (sqlite-rs, #134).
        let base = self.cursor(slot).map(|c| c.sequence_base()).unwrap_or(0);
        if idx >= self.sequences.len() {
            self.sequences.resize(idx.saturating_add(1), None);
        }
        let Some(cell) = self.sequences.get_mut(idx) else {
            return Ok(base);
        };
        let value = cell.unwrap_or(base);
        *cell = Some(value.saturating_add(1));
        Ok(value)
    }

    fn schema_storage(
        &mut self,
        opcode: &'static str,
    ) -> Result<&mut Box<dyn SchemaStorage>, ExecError> {
        self.schema_storage
            .as_mut()
            .ok_or(ExecError::SchemaStorageMissing { opcode })
    }

    /// Records `root` as the root page cursor slot `slot` was last
    /// opened against -- `OpenDup`'s lookup key (db-core#125).
    fn set_cursor_root(&mut self, slot: i32, root: u32) -> Result<(), ExecError> {
        let idx = Self::index("cursor slot write", slot)?;
        if idx >= self.cursor_roots.len() {
            self.cursor_roots.resize(idx.saturating_add(1), None);
        }
        if let Some(cell) = self.cursor_roots.get_mut(idx) {
            *cell = Some(root);
        }
        Ok(())
    }

    fn cursor_root(&self, slot: i32) -> Result<u32, ExecError> {
        let idx = Self::index("cursor slot read", slot)?;
        self.cursor_roots
            .get(idx)
            .copied()
            .flatten()
            .ok_or(ExecError::CursorNotOpen { slot })
    }

    fn param(&self, index: i32) -> Option<&Value> {
        let idx = usize::try_from(index).ok()?.checked_sub(1)?;
        self.params.get(idx)
    }

    #[allow(clippy::cast_sign_loss)]
    fn index(opcode: &'static str, reg: i32) -> Result<usize, ExecError> {
        if reg < 0 || reg as usize > MAX_REGISTERS {
            return Err(ExecError::RegisterOutOfRange { opcode, index: reg });
        }
        Ok(reg as usize)
    }

    pub(crate) fn bounded_count(opcode: &'static str, count: i32) -> Result<usize, ExecError> {
        if !(0..=MAX_REGISTERS as i32).contains(&count) {
            return Err(ExecError::RegisterRangeTooLarge { opcode, count });
        }
        #[allow(clippy::cast_sign_loss)]
        Ok(count as usize)
    }

    /// Reads register `reg`. An unwritten register reads as NULL.
    pub fn register(&self, reg: i32) -> Result<&Value, ExecError> {
        let idx = Self::index("register read", reg)?;
        Ok(self.registers.get(idx).unwrap_or(&Value::Null))
    }

    /// Writes register `reg`, growing the register file with NULL
    /// filler as needed.
    pub fn set_register(&mut self, reg: i32, value: Value) -> Result<(), ExecError> {
        let idx = Self::index("register write", reg)?;
        if idx >= self.registers.len() {
            self.registers.resize(idx.saturating_add(1), Value::Null);
        }
        if let Some(slot) = self.registers.get_mut(idx) {
            *slot = value;
        }
        Ok(())
    }

    /// Takes register `reg`'s value, leaving NULL behind, without
    /// cloning (`ResultRow`'s hand-off).
    fn take_register(&mut self, reg: i32) -> Result<Value, ExecError> {
        let idx = Self::index("register read", reg)?;
        Ok(match self.registers.get_mut(idx) {
            Some(slot) => std::mem::replace(slot, Value::Null),
            None => Value::Null,
        })
    }

    /// Opens cursor slot `slot` with an arbitrary [`Cursor`]
    /// implementation -- the storage-agnostic wiring point ADR 0008
    /// calls for. Not an opcode: `OpenRead`'s real root-page/pager
    /// semantics are future work (`cursor.rs`'s db-storage wiring); a
    /// caller sets a program's cursors up via this method before
    /// running it.
    fn set_pseudo_reg(&mut self, slot: i32, reg: Option<i32>) -> Result<(), ExecError> {
        let idx = Self::index("cursor slot write", slot)?;
        if idx >= self.pseudo_regs.len() {
            self.pseudo_regs.resize(idx.saturating_add(1), None);
        }
        if let Some(cell) = self.pseudo_regs.get_mut(idx) {
            *cell = reg;
        }
        Ok(())
    }

    fn pseudo_reg(&self, slot: i32) -> Option<i32> {
        usize::try_from(slot)
            .ok()
            .and_then(|idx| self.pseudo_regs.get(idx).copied().flatten())
    }

    /// `Column` on a pseudo cursor: decode `col` out of the bound
    /// register's current record blob (re-read on every call, since the
    /// register is rewritten between rows — `SorterData`/`MakeRecord`).
    fn pseudo_column(
        &self,
        reg: i32,
        col: usize,
        opcode: &'static str,
    ) -> Result<Value, ExecError> {
        match self.register(reg)? {
            Value::Blob(bytes) => {
                Ok(decode_column(bytes, col, self.text_encoding).unwrap_or(Value::Null))
            }
            Value::Null => Ok(Value::Null),
            other => Err(ExecError::MalformedInstruction {
                opcode,
                reason: format!("pseudo-cursor register holds {other:?}, not a record blob"),
            }),
        }
    }

    pub fn open_cursor(&mut self, slot: i32, cursor: Box<dyn Cursor>) -> Result<(), ExecError> {
        self.set_pseudo_reg(slot, None)?;
        let idx = Self::index("cursor slot write", slot)?;
        if idx >= self.cursors.len() {
            self.cursors.resize_with(idx.saturating_add(1), || None);
        }
        if let Some(cell) = self.cursors.get_mut(idx) {
            *cell = Some(cursor);
        }
        Ok(())
    }

    fn cursor(&self, slot: i32) -> Result<&dyn Cursor, ExecError> {
        let idx = Self::index("cursor slot read", slot)?;
        self.cursors
            .get(idx)
            .and_then(Option::as_ref)
            .map(std::convert::AsRef::as_ref)
            .ok_or(ExecError::CursorNotOpen { slot })
    }

    fn cursor_mut(&mut self, slot: i32) -> Result<&mut Box<dyn Cursor>, ExecError> {
        let idx = Self::index("cursor slot write", slot)?;
        self.cursors
            .get_mut(idx)
            .and_then(Option::as_mut)
            .ok_or(ExecError::CursorNotOpen { slot })
    }

    /// Reads aggregate-context slot `slot`. `None` if no `AggStep` has
    /// run for this slot yet (or the group is empty) -- distinct from
    /// `cursor`'s error-on-unopened behavior, since an unaggregated
    /// slot is a legitimate zero-row state, not a malformed program.
    fn agg_context(&self, slot: i32) -> Result<Option<&AggState>, ExecError> {
        let idx = Self::index("agg context read", slot)?;
        Ok(self.agg_contexts.get(idx).and_then(Option::as_ref))
    }

    /// Writes aggregate-context slot `slot`, growing the table with
    /// empty filler as needed -- mirrors `open_cursor`'s growth policy,
    /// into the disjoint `agg_contexts` storage.
    fn set_agg_context(&mut self, slot: i32, value: AggState) -> Result<(), ExecError> {
        let idx = Self::index("agg context write", slot)?;
        if idx >= self.agg_contexts.len() {
            self.agg_contexts
                .resize_with(idx.saturating_add(1), || None);
        }
        if let Some(cell) = self.agg_contexts.get_mut(idx) {
            *cell = Some(value);
        }
        Ok(())
    }

    /// Clears aggregate-context slot `slot` back to `None` -- used by
    /// `AggFinal` so a slot's leftover accumulator from one invocation
    /// can't leak into a later invocation that skips `AggStep` entirely
    /// (a zero-row group, or a correlated aggregate subquery re-run
    /// once per outer row).
    fn clear_agg_context(&mut self, slot: i32) -> Result<(), ExecError> {
        let idx = Self::index("agg context clear", slot)?;
        if let Some(cell) = self.agg_contexts.get_mut(idx) {
            *cell = None;
        }
        Ok(())
    }

    /// Appends `row` to the set of rows produced so far.
    pub fn emit_row(&mut self, row: Vec<Value>) {
        self.rows.push(row);
    }

    /// The rows emitted by the program so far, in emission order.
    pub fn rows(&self) -> &[Vec<Value>] {
        &self.rows
    }
}

/// Compare opcodes (`Eq`/`Ge`/`Gt`/`Le`/`Lt`): jump to `p2` if `r[p1]
/// <op> r[p3]` holds. Either operand NULL means unknown, so no jump is
/// taken. `p4`, if [`P4::CollSeq`], selects collation/affinity for the
/// comparison; absent `p4` defaults to BINARY collation, BLOB affinity
/// (no coercion).
fn compare_jump(
    vm: &Vm,
    instr: &Instruction,
    holds: fn(Ordering) -> bool,
) -> Result<Step, ExecError> {
    let a = vm.register(instr.p1)?;
    let b = vm.register(instr.p3)?;
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Ok(Step::Next);
    }
    let (collation, affinity) = match &instr.p4 {
        P4::CollSeq {
            collation,
            affinity,
        } => (*collation, Affinity::from_p4_byte(*affinity)),
        _ => (Collation::Binary, Affinity::Blob),
    };
    let ord = if matches!(affinity, Affinity::Blob) {
        compare(a, b, collation)
    } else {
        let mut a = a.clone();
        let mut b = b.clone();
        apply_affinity(&mut a, affinity);
        apply_affinity(&mut b, affinity);
        compare(&a, &b, collation)
    };
    Ok(if holds(ord) {
        Step::Jump(to_pc(instr.p2))
    } else {
        Step::Next
    })
}

fn real_affinity(vm: &mut Vm, instr: &Instruction) -> Result<Step, ExecError> {
    let mut v = vm.take_register(instr.p1)?;
    apply_affinity(&mut v, Affinity::Real);
    vm.set_register(instr.p1, v)?;
    Ok(Step::Next)
}

fn cast(vm: &mut Vm, instr: &Instruction) -> Result<Step, ExecError> {
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let affinity = Affinity::from_p4_byte(instr.p2 as u8);
    let v = vm.take_register(instr.p1)?;
    vm.set_register(instr.p1, cast_to(&v, affinity))?;
    Ok(Step::Next)
}

fn binary_op(
    vm: &mut Vm,
    instr: &Instruction,
    op: fn(&Value, &Value) -> Value,
) -> Result<Step, ExecError> {
    let result = {
        let a = vm.register(instr.p1)?;
        let b = vm.register(instr.p2)?;
        if matches!(a, Value::Null) || matches!(b, Value::Null) {
            Value::Null
        } else {
            op(a, b)
        }
    };
    vm.set_register(instr.p3, result)?;
    Ok(Step::Next)
}

/// `p2`-op-`p1` binary opcodes (sqlite-rs's operand order for
/// `Subtract`/`Divide`/`Remainder`/`ShiftLeft`/`ShiftRight`/`Concat`).
fn binary_op_reversed(
    vm: &mut Vm,
    instr: &Instruction,
    op: fn(&Value, &Value) -> Value,
) -> Result<Step, ExecError> {
    let result = {
        let a = vm.register(instr.p1)?;
        let b = vm.register(instr.p2)?;
        if matches!(a, Value::Null) || matches!(b, Value::Null) {
            Value::Null
        } else {
            op(b, a)
        }
    };
    vm.set_register(instr.p3, result)?;
    Ok(Step::Next)
}

fn arith_not(vm: &mut Vm, instr: &Instruction) -> Result<Step, ExecError> {
    let result = match vm.register(instr.p1)? {
        Value::Null => Value::Null,
        other => Value::Integer(i64::from(is_falsy(other))),
    };
    vm.set_register(instr.p2, result)?;
    Ok(Step::Next)
}

fn bit_not(vm: &mut Vm, instr: &Instruction) -> Result<Step, ExecError> {
    let result = match vm.register(instr.p1)? {
        Value::Null => Value::Null,
        other => coerce::bit_not(other),
    };
    vm.set_register(instr.p2, result)?;
    Ok(Step::Next)
}

/// `Count`: the cursor's own count when it has one, else a full scan.
fn count_rows(cursor: &mut dyn Cursor) -> i64 {
    if let Some(n) = cursor.count() {
        return n;
    }
    let mut n: i64 = 0;
    let mut has_row = cursor.rewind();
    while has_row {
        n = n.saturating_add(1);
        has_row = cursor.next();
    }
    n
}

fn register_as_i64(vm: &Vm, reg: i32) -> Result<i64, ExecError> {
    match vm.register(reg)? {
        Value::Integer(i) => Ok(*i),
        other => Err(ExecError::TypeMismatch {
            opcode: "control counter",
            found: value_kind(other),
        }),
    }
}

/// Reads `p4`'s register-count (`P4::Int(n)`) worth of values starting
/// at register `first_reg`, cloned into a `Vec` -- the shared key-read
/// convention `Found`/`IdxLE`/`SeekIndexEq`/`SeekIndexGE`/
/// `IdxCompareGT`/`NoConflict`'s docs describe as "a key from registers
/// `p3..p3+p4`" (db-core#126), mirroring `IdxInsert`/`IdxDelete`'s
/// existing identical read.
/// The per-column collations an index-key opcode's `P4` names (sqlite-rs
/// `cursor::seek_key_collations`, #134): `P4::SeekKey(c)` is `c`;
/// `P4::Int(n)` is `n` BINARY columns.
fn seek_key_collations(opcode: &'static str, p4: &P4) -> Result<Vec<Collation>, ExecError> {
    match p4 {
        P4::SeekKey(collations) => Ok(collations.clone()),
        P4::Int(n) => {
            let count = Vm::bounded_count(
                opcode,
                i32::try_from(*n).map_err(|_| ExecError::MalformedInstruction {
                    opcode,
                    reason: format!("key count {n} does not fit in p4"),
                })?,
            )?;
            Ok(vec![Collation::Binary; count])
        }
        other => Err(ExecError::MalformedInstruction {
            opcode,
            reason: format!("expected a SeekKey or Int P4 (key columns), got {other:?}"),
        }),
    }
}

/// `count` consecutive registers starting at `first_reg`.
fn register_range(
    vm: &Vm,
    opcode: &'static str,
    first_reg: i32,
    count: usize,
) -> Result<Vec<Value>, ExecError> {
    let mut values = Vec::with_capacity(count);
    for i in 0..count {
        let reg = first_reg
            .checked_add(i32::try_from(i).unwrap_or(i32::MAX))
            .ok_or(ExecError::RegisterOutOfRange {
                opcode,
                index: first_reg,
            })?;
        values.push(vm.register(reg)?.clone());
    }
    Ok(values)
}

/// An index-key operand: the key values from `first_reg` plus the
/// collations `p4` names (one per key column).
fn key_from_registers(
    vm: &Vm,
    opcode: &'static str,
    first_reg: i32,
    p4: &P4,
) -> Result<(Vec<Value>, Vec<Collation>), ExecError> {
    let collations = seek_key_collations(opcode, p4)?;
    let count = collations.len();
    let mut key = Vec::with_capacity(count);
    for i in 0..count {
        let reg = first_reg
            .checked_add(i32::try_from(i).unwrap_or(i32::MAX))
            .ok_or(ExecError::RegisterOutOfRange {
                opcode,
                index: first_reg,
            })?;
        key.push(vm.register(reg)?.clone());
    }
    Ok((key, collations))
}

fn in_i64_range(r: f64) -> bool {
    r >= i64::MIN as f64 && r < i64::MAX as f64
}

fn try_to_integer(v: &Value) -> Option<i64> {
    match v {
        Value::Integer(i) => Some(*i),
        #[allow(clippy::cast_possible_truncation)]
        Value::Real(r) if r.fract() == 0.0 && r.is_finite() && in_i64_range(*r) => Some(*r as i64),
        Value::Text(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// Parses a `"name(arity)"` descriptor (e.g. `"count(0)"`,
/// `"sum(1)"`) into its parts, used by `AggFinal`'s `p4`.
fn parse_function_descriptor(descriptor: &str) -> Option<(&str, usize)> {
    let open = descriptor.find('(')?;
    if !descriptor.ends_with(')') {
        return None;
    }
    let name = descriptor.get(..open)?;
    let inner_start = open.checked_add(1)?;
    let inner_end = descriptor.len().checked_sub(1)?;
    let arity: usize = descriptor.get(inner_start..inner_end)?.parse().ok()?;
    Some((name, arity))
}

/// Executes one instruction against `vm`, returning where control flow
/// goes next. `pc` is this instruction's own address (needed by
/// `Once`'s per-address guard).
#[allow(clippy::too_many_lines)]
fn step(vm: &mut Vm, pc: usize, instr: &Instruction) -> Result<Step, ExecError> {
    match instr.opcode {
        Opcode::Init => Ok(if instr.p2 == 0 {
            Step::Next
        } else {
            Step::Jump(to_pc(instr.p2))
        }),
        Opcode::Goto => Ok(Step::Jump(to_pc(instr.p2))),
        Opcode::Once => {
            if vm.once_fired.insert(pc) {
                Ok(Step::Next)
            } else {
                Ok(Step::Jump(to_pc(instr.p2)))
            }
        }
        Opcode::BeginSubrtn => Ok(Step::Next),
        Opcode::Return => match vm.register(instr.p1)? {
            Value::Integer(i) => match i32::try_from(*i) {
                Ok(target) => Ok(Step::Jump(to_pc(target))),
                Err(_) => Err(ExecError::MalformedInstruction {
                    opcode: "Return",
                    reason: format!("return address {i} does not fit in a PC"),
                }),
            },
            other => Err(ExecError::TypeMismatch {
                opcode: "Return",
                found: value_kind(other),
            }),
        },
        Opcode::Halt => {
            let message = match &instr.p4 {
                P4::Str(s) => Some(s.clone()),
                _ => None,
            };
            Ok(Step::Halt {
                code: instr.p1,
                message,
            })
        }
        Opcode::IsNull => {
            let jump = matches!(vm.register(instr.p1)?, Value::Null);
            Ok(if jump {
                Step::Jump(to_pc(instr.p2))
            } else {
                Step::Next
            })
        }
        Opcode::NotNull => {
            let jump = !matches!(vm.register(instr.p1)?, Value::Null);
            Ok(if jump {
                Step::Jump(to_pc(instr.p2))
            } else {
                Step::Next
            })
        }
        Opcode::IfNot => {
            let v = vm.register(instr.p1)?;
            let take_jump = match v {
                Value::Null => instr.p3 != 0,
                other => is_falsy(other),
            };
            Ok(if take_jump {
                Step::Jump(to_pc(instr.p2))
            } else {
                Step::Next
            })
        }
        Opcode::MustBeInt => {
            let v = vm.register(instr.p1)?.clone();
            match try_to_integer(&v) {
                Some(i) => {
                    vm.set_register(instr.p1, Value::Integer(i))?;
                    Ok(Step::Next)
                }
                None if instr.p2 != 0 => Ok(Step::Jump(to_pc(instr.p2))),
                None => Err(ExecError::MustBeInt),
            }
        }
        Opcode::OffsetLimit => {
            let limit = register_as_i64(vm, instr.p1)?;
            let offset = register_as_i64(vm, instr.p3)?;
            let combined = if limit > 0 {
                limit.saturating_add(offset.max(0))
            } else {
                -1
            };
            vm.set_register(instr.p2, Value::Integer(combined))?;
            Ok(Step::Next)
        }
        Opcode::IfPos => {
            let v = register_as_i64(vm, instr.p1)?;
            if v > 0 {
                vm.set_register(
                    instr.p1,
                    Value::Integer(v.saturating_sub(i64::from(instr.p3))),
                )?;
                Ok(Step::Jump(to_pc(instr.p2)))
            } else {
                Ok(Step::Next)
            }
        }
        Opcode::IfNotZero => {
            let v = register_as_i64(vm, instr.p1)?;
            if v != 0 {
                if v > 0 {
                    vm.set_register(instr.p1, Value::Integer(v.saturating_sub(1)))?;
                }
                Ok(Step::Jump(to_pc(instr.p2)))
            } else {
                Ok(Step::Next)
            }
        }
        Opcode::DecrJumpZero => {
            let v = register_as_i64(vm, instr.p1)?.saturating_sub(1);
            vm.set_register(instr.p1, Value::Integer(v))?;
            Ok(if v == 0 {
                Step::Jump(to_pc(instr.p2))
            } else {
                Step::Next
            })
        }

        Opcode::Eq => compare_jump(vm, instr, |o| o == Ordering::Equal),
        Opcode::Ge => compare_jump(vm, instr, |o| o != Ordering::Less),
        Opcode::Gt => compare_jump(vm, instr, |o| o == Ordering::Greater),
        Opcode::Le => compare_jump(vm, instr, |o| o != Ordering::Greater),
        Opcode::Lt => compare_jump(vm, instr, |o| o == Ordering::Less),
        Opcode::RealAffinity => real_affinity(vm, instr),
        Opcode::Cast => cast(vm, instr),

        Opcode::Add => binary_op(vm, instr, coerce::checked_add),
        Opcode::Subtract => binary_op_reversed(vm, instr, coerce::checked_sub),
        Opcode::Multiply => binary_op(vm, instr, coerce::checked_mul),
        Opcode::Divide => binary_op_reversed(vm, instr, coerce::checked_div),
        Opcode::Remainder => binary_op_reversed(vm, instr, coerce::checked_rem),
        Opcode::Not => arith_not(vm, instr),
        Opcode::BitAnd => binary_op(vm, instr, coerce::bit_and),
        Opcode::BitOr => binary_op(vm, instr, coerce::bit_or),
        Opcode::ShiftLeft => binary_op_reversed(vm, instr, coerce::shift_left),
        Opcode::ShiftRight => binary_op_reversed(vm, instr, coerce::shift_right),
        Opcode::BitNot => bit_not(vm, instr),
        Opcode::Concat => binary_op_reversed(vm, instr, coerce::concat),

        Opcode::Integer => {
            vm.set_register(instr.p2, Value::Integer(i64::from(instr.p1)))?;
            Ok(Step::Next)
        }
        Opcode::Int64 => {
            let i = match &instr.p4 {
                P4::Int(i) => *i,
                other => {
                    return Err(ExecError::MalformedInstruction {
                        opcode: "Int64",
                        reason: format!("expected an integer P4, got {other:?}"),
                    })
                }
            };
            vm.set_register(instr.p2, Value::Integer(i))?;
            Ok(Step::Next)
        }
        Opcode::Real => {
            let r = match &instr.p4 {
                P4::Real(r) => *r,
                other => {
                    return Err(ExecError::MalformedInstruction {
                        opcode: "Real",
                        reason: format!("expected a real P4, got {other:?}"),
                    })
                }
            };
            vm.set_register(instr.p2, Value::Real(r))?;
            Ok(Step::Next)
        }
        Opcode::Blob => {
            let bytes = match &instr.p4 {
                P4::Blob(bytes) => bytes.clone(),
                other => {
                    return Err(ExecError::MalformedInstruction {
                        opcode: "Blob",
                        reason: format!("expected a blob P4, got {other:?}"),
                    })
                }
            };
            vm.set_register(instr.p2, Value::Blob(bytes.into()))?;
            Ok(Step::Next)
        }
        Opcode::Null => {
            let last = instr.p3.max(instr.p2);
            for reg in instr.p2..=last {
                vm.set_register(reg, Value::Null)?;
            }
            Ok(Step::Next)
        }
        Opcode::String8 => {
            let s = match &instr.p4 {
                P4::Str(s) => s.clone(),
                other => {
                    return Err(ExecError::MalformedInstruction {
                        opcode: "String8",
                        reason: format!("expected a string P4, got {other:?}"),
                    })
                }
            };
            vm.set_register(instr.p2, Value::Text(s.into()))?;
            Ok(Step::Next)
        }
        Opcode::Variable => {
            let value = vm.param(instr.p1).cloned().unwrap_or(Value::Null);
            vm.set_register(instr.p2, value)?;
            Ok(Step::Next)
        }
        Opcode::Copy => {
            let value = vm.register(instr.p1)?.clone();
            vm.set_register(instr.p2, value)?;
            Ok(Step::Next)
        }
        Opcode::MakeRecord => {
            let count = Vm::bounded_count("MakeRecord", instr.p2)?;
            let affinities: &[u8] = match &instr.p4 {
                P4::Affinity(bytes) => bytes,
                _ => &[],
            };
            let mut values = Vec::with_capacity(count);
            for i in 0..count {
                let reg = instr
                    .p1
                    .checked_add(i as i32)
                    .ok_or(ExecError::RegisterOutOfRange {
                        opcode: "MakeRecord",
                        index: instr.p1,
                    })?;
                let mut value = vm.register(reg)?.clone();
                if let Some(byte) = affinities.get(i) {
                    apply_affinity(&mut value, Affinity::from_p4_byte(*byte));
                }
                values.push(value);
            }
            let payload = encode_record(&values, TextEncoding::Utf8);
            vm.set_register(instr.p3, Value::Blob(payload.into()))?;
            Ok(Step::Next)
        }
        Opcode::ResultRow => {
            let count = Vm::bounded_count("ResultRow", instr.p2)?;
            let mut row = Vec::with_capacity(count);
            for i in 0..count {
                let reg = instr
                    .p1
                    .checked_add(i as i32)
                    .ok_or(ExecError::RegisterOutOfRange {
                        opcode: "ResultRow",
                        index: instr.p1,
                    })?;
                row.push(vm.take_register(reg)?);
            }
            vm.emit_row(row);
            Ok(Step::Next)
        }

        Opcode::Rewind => {
            vm.clear_null_row(instr.p1)?;
            let has_row = vm.cursor_mut(instr.p1)?.rewind();
            Ok(if has_row {
                Step::Next
            } else {
                Step::Jump(to_pc(instr.p2))
            })
        }
        Opcode::Next => {
            vm.clear_null_row(instr.p1)?;
            let has_row = vm.cursor_mut(instr.p1)?.next();
            Ok(if has_row {
                Step::Jump(to_pc(instr.p2))
            } else {
                Step::Next
            })
        }
        Opcode::Last => {
            vm.clear_null_row(instr.p1)?;
            let has_row = vm.cursor_mut(instr.p1)?.last();
            Ok(if has_row {
                Step::Next
            } else {
                Step::Jump(to_pc(instr.p2))
            })
        }
        Opcode::Column => {
            #[allow(clippy::cast_sign_loss)]
            let col = instr.p2 as usize;
            let value = if vm.is_null_row(instr.p1)? {
                Value::Null
            } else if let Some(reg) = vm.pseudo_reg(instr.p1) {
                vm.pseudo_column(reg, col, "Column")?
            } else {
                vm.cursor(instr.p1)?.column(col)
            };
            vm.set_register(instr.p3, value)?;
            Ok(Step::Next)
        }
        Opcode::Rowid => {
            let rowid = if vm.is_null_row(instr.p1)? {
                0
            } else {
                vm.cursor(instr.p1)?.rowid()
            };
            vm.set_register(instr.p2, Value::Integer(rowid))?;
            Ok(Step::Next)
        }
        Opcode::SeekRowid => {
            vm.clear_null_row(instr.p1)?;
            let rowid = register_as_i64(vm, instr.p3)?;
            let found = vm.cursor_mut(instr.p1)?.seek(rowid);
            Ok(if found {
                Step::Next
            } else {
                Step::Jump(to_pc(instr.p2))
            })
        }
        Opcode::NullRow => {
            vm.set_null_row(instr.p1)?;
            Ok(Step::Next)
        }
        Opcode::Sequence => {
            let value = vm.next_sequence(instr.p1)?;
            vm.set_register(instr.p2, Value::Integer(value))?;
            Ok(Step::Next)
        }
        Opcode::Count => {
            // sqlite-rs: p1 is the *root page* of the table to count (the
            // `SELECT count(*)` fast path opens its own cursor); with no
            // cursor factory (in-memory programs) p1 names an open slot.
            let count = if let Some(factory) = vm.cursor_factory.as_deref_mut() {
                #[allow(clippy::cast_sign_loss)]
                let root = instr.p1 as u32;
                let mut cursor = factory
                    .open_read(root)
                    .map_err(ExecError::CursorFactoryFailed)?;
                count_rows(cursor.as_mut())
            } else {
                count_rows(vm.cursor_mut(instr.p1)?.as_mut())
            };
            vm.set_register(instr.p2, Value::Integer(count))?;
            Ok(Step::Next)
        }
        Opcode::AutoIndexInsert => {
            // sqlite-rs: p2 = first key register (count/collations from P4),
            // p3 = rowid register.
            let (key, collations) = key_from_registers(vm, "AutoIndexInsert", instr.p2, &instr.p4)?;
            let rowid = register_as_i64(vm, instr.p3)?;
            if vm.cursor(instr.p1).is_err() {
                vm.open_cursor(instr.p1, Box::new(super::cursor::AutoIndexCursor::new()))?;
            }
            let ok = vm
                .cursor_mut(instr.p1)?
                .auto_index_insert(key, &collations, rowid);
            if !ok {
                return Err(ExecError::MalformedInstruction {
                    opcode: "AutoIndexInsert",
                    reason: "cursor slot is not an automatic-index cursor".to_string(),
                });
            }
            Ok(Step::Next)
        }
        Opcode::AutoIndexSeek => {
            let (key, collations) = key_from_registers(vm, "AutoIndexSeek", instr.p3, &instr.p4)?;
            let found = vm.cursor_mut(instr.p1)?.auto_index_seek(&key, &collations);
            Ok(if found {
                Step::Next
            } else {
                Step::Jump(to_pc(instr.p2))
            })
        }
        Opcode::AutoIndexRowid => {
            let rowid = vm.cursor(instr.p1)?.rowid();
            vm.set_register(instr.p2, Value::Integer(rowid))?;
            Ok(Step::Next)
        }
        Opcode::AutoIndexNext => {
            let has_next = vm.cursor_mut(instr.p1)?.auto_index_next();
            Ok(if has_next {
                Step::Jump(to_pc(instr.p2))
            } else {
                Step::Next
            })
        }
        Opcode::OpenRead | Opcode::OpenWrite => {
            if instr.p3 != 0 {
                return Err(ExecError::AttachedDatabasesUnsupported);
            }
            let Some(factory) = vm.cursor_factory.as_deref_mut() else {
                // No factory installed -- fall back to the pre-wired
                // path: assert the caller already wired this slot via
                // `open_cursor` before running the program.
                vm.cursor(instr.p1)?;
                return Ok(Step::Next);
            };
            #[allow(clippy::cast_sign_loss)]
            let root = instr.p2 as u32;
            // sqlite-rs flags an index cursor with `p5 != 0` (no key
            // descriptor); a `P4::SortKey` carries one when the planner has it.
            let cursor = match (&instr.p4, instr.p5, instr.opcode) {
                (P4::SortKey(key), _, _) => factory.open_index(root, key),
                (_, p5, _) if p5 != 0 => factory.open_index(root, &[]),
                (_, _, Opcode::OpenWrite) => factory.open_write(root),
                _ => factory.open_read(root),
            }
            .map_err(ExecError::CursorFactoryFailed)?;
            vm.open_cursor(instr.p1, cursor)?;
            vm.set_cursor_root(instr.p1, root)?;
            Ok(Step::Next)
        }
        Opcode::OpenDup => {
            // sqlite-rs: a second cursor sharing an ephemeral table's rows.
            if let Some(dup) = vm.cursor(instr.p2)?.dup() {
                vm.open_cursor(instr.p1, dup)?;
                return Ok(Step::Next);
            }
            let root = vm.cursor_root(instr.p2)?;
            let factory =
                vm.cursor_factory
                    .as_deref_mut()
                    .ok_or(ExecError::CursorFactoryFailed(CursorFactoryError(
                        "OpenDup requires a cursor factory".to_string(),
                    )))?;
            let cursor = factory
                .open_read(root)
                .map_err(ExecError::CursorFactoryFailed)?;
            vm.open_cursor(instr.p1, cursor)?;
            vm.set_cursor_root(instr.p1, root)?;
            Ok(Step::Next)
        }
        Opcode::OpenPseudo => {
            // sqlite-rs: the cursor reads register p2 *lazily* — codegen
            // opens it before the register holds a row (`SorterData` /
            // `MakeRecord` fill it later, and rewrite it between rows).
            vm.open_cursor(instr.p1, Box::new(PseudoCursor::new(&[])))?;
            vm.set_pseudo_reg(instr.p1, Some(instr.p2))?;
            Ok(Step::Next)
        }
        Opcode::OpenEphemeral => {
            // sqlite-rs `cursor::open_ephemeral`: p5 = 0 -> ephemeral index
            // (DISTINCT / IN-subquery), 1 -> ephemeral table (materialized
            // rows by rowid), 2 -> automatic join index.
            let cursor: Box<dyn Cursor> = match instr.p5 {
                0 => Box::new(super::cursor::EphemeralIndexCursor::new()),
                2 => Box::new(super::cursor::AutoIndexCursor::new()),
                _ => Box::new(EphemeralTableCursor::new()),
            };
            vm.open_cursor(instr.p1, cursor)?;
            Ok(Step::Next)
        }
        Opcode::SorterOpen => {
            let keys = match &instr.p4 {
                P4::SortKey(keys) => keys.clone(),
                other => {
                    return Err(ExecError::MalformedInstruction {
                        opcode: "SorterOpen",
                        reason: format!("expected a SortKey P4, got {other:?}"),
                    })
                }
            };
            let bound = if instr.p5 == 0 {
                None
            } else {
                match vm.register(instr.p2)? {
                    Value::Integer(n) if *n >= 0 => usize::try_from(*n).ok(),
                    _ => None,
                }
            };
            vm.open_cursor(instr.p1, Box::new(SorterCursor::new(keys, bound)))?;
            Ok(Step::Next)
        }
        Opcode::SorterInsert => {
            let blob = match vm.register(instr.p2)? {
                Value::Blob(bytes) => bytes.clone(),
                other => {
                    return Err(ExecError::TypeMismatch {
                        opcode: "SorterInsert",
                        found: value_kind(other),
                    })
                }
            };
            let inserted = vm.cursor_mut(instr.p1)?.sorter_insert(blob);
            if !inserted {
                return Err(ExecError::MalformedInstruction {
                    opcode: "SorterInsert",
                    reason: "cursor slot is not a sorter".to_string(),
                });
            }
            Ok(Step::Next)
        }
        Opcode::SorterSort | Opcode::Sort | Opcode::HashAggRewind => {
            let has_row = vm.cursor_mut(instr.p1)?.rewind();
            Ok(if has_row {
                Step::Next
            } else {
                Step::Jump(to_pc(instr.p2))
            })
        }
        Opcode::SorterNext | Opcode::HashAggNext => {
            let has_row = vm.cursor_mut(instr.p1)?.next();
            Ok(if has_row {
                Step::Jump(to_pc(instr.p2))
            } else {
                Step::Next
            })
        }
        Opcode::SorterData => {
            let blob =
                vm.cursor(instr.p1)?
                    .current_blob()
                    .ok_or(ExecError::MalformedInstruction {
                        opcode: "SorterData",
                        reason: "sorter has not been sorted yet, or has no current row".to_string(),
                    })?;
            vm.set_register(instr.p2, blob)?;
            Ok(Step::Next)
        }
        Opcode::HashAggOpen => {
            let keys = match &instr.p4 {
                P4::GroupKey(keys) => keys.clone(),
                other => {
                    return Err(ExecError::MalformedInstruction {
                        opcode: "HashAggOpen",
                        reason: format!("expected a GroupKey P4, got {other:?}"),
                    })
                }
            };
            vm.open_cursor(instr.p1, Box::new(HashAggCursor::new(keys)))?;
            Ok(Step::Next)
        }
        Opcode::HashAggFind => {
            let blob = match vm.register(instr.p2)? {
                Value::Blob(bytes) => bytes.clone(),
                other => {
                    return Err(ExecError::TypeMismatch {
                        opcode: "HashAggFind",
                        found: value_kind(other),
                    })
                }
            };
            let found = vm.cursor_mut(instr.p1)?.hash_agg_find(blob);
            if !found {
                return Err(ExecError::MalformedInstruction {
                    opcode: "HashAggFind",
                    reason: "cursor slot is not a hash-aggregation cursor".to_string(),
                });
            }
            Ok(Step::Next)
        }
        Opcode::HashAggStep => {
            let (name, arity, collation) = match &instr.p4 {
                P4::AggFunc {
                    name,
                    arity,
                    collation,
                } => (name.as_str(), *arity, *collation),
                other => {
                    return Err(ExecError::MalformedInstruction {
                        opcode: "HashAggStep",
                        reason: format!("expected an AggFunc P4, got {other:?}"),
                    })
                }
            };
            let mut args = Vec::with_capacity(arity);
            for i in 0..arity {
                let reg = instr
                    .p2
                    .checked_add(i32::try_from(i).unwrap_or(i32::MAX))
                    .ok_or(ExecError::RegisterOutOfRange {
                        opcode: "HashAggStep",
                        index: instr.p2,
                    })?;
                args.push(vm.register(reg)?.clone());
            }
            #[allow(clippy::cast_sign_loss)]
            let slot = instr.p1.max(0) as usize;
            let handled = vm
                .cursor_mut(instr.p3)?
                .hash_agg_step(slot, name, &args, collation)
                .map_err(|e| ExecError::MalformedInstruction {
                    opcode: "HashAggStep",
                    reason: e.to_string(),
                })?;
            if !handled {
                return Err(ExecError::MalformedInstruction {
                    opcode: "HashAggStep",
                    reason: "cursor slot has no current group".to_string(),
                });
            }
            Ok(Step::Next)
        }
        Opcode::HashAggData => {
            let blob =
                vm.cursor(instr.p1)?
                    .current_blob()
                    .ok_or(ExecError::MalformedInstruction {
                    opcode: "HashAggData",
                    reason:
                        "hash-aggregation cursor has not been rewound yet, or has no current group"
                            .to_string(),
                })?;
            let accumulators: Vec<Option<AggState>> = vm
                .cursor(instr.p1)?
                .hash_agg_group_accumulators()
                .map(<[_]>::to_vec)
                .unwrap_or_default();
            for (slot, state) in accumulators.into_iter().enumerate() {
                let slot = i32::try_from(slot).unwrap_or(i32::MAX);
                match state {
                    Some(state) => vm.set_agg_context(slot, state)?,
                    None => vm.clear_agg_context(slot)?,
                }
            }
            vm.set_register(instr.p2, blob)?;
            Ok(Step::Next)
        }
        Opcode::Insert => {
            let rowid = match vm.register(instr.p2)? {
                Value::Integer(i) => *i,
                other => {
                    return Err(ExecError::TypeMismatch {
                        opcode: "Insert",
                        found: value_kind(other),
                    })
                }
            };
            let payload = match vm.register(instr.p3)? {
                Value::Blob(bytes) => bytes.clone(),
                other => {
                    return Err(ExecError::TypeMismatch {
                        opcode: "Insert",
                        found: value_kind(other),
                    })
                }
            };
            let encoding = vm.text_encoding;
            let cursor = vm.cursor_mut(instr.p1)?;
            // A storage-backed cursor takes the record bytes as they are
            // (byte-exact with MakeRecord); in-memory ones decode them.
            let inserted = match cursor.insert_payload(rowid, &payload) {
                Some(ok) => ok,
                None => {
                    let values = decode_record(&payload, encoding).map_err(|e| {
                        ExecError::MalformedInstruction {
                            opcode: "Insert",
                            reason: e.to_string(),
                        }
                    })?;
                    cursor.insert(rowid, values)
                }
            };
            if !inserted {
                return Err(ExecError::MalformedInstruction {
                    opcode: "Insert",
                    reason: "cursor slot does not support insertion".to_string(),
                });
            }
            Ok(Step::Next)
        }

        Opcode::Function => {
            let descriptor = match &instr.p4 {
                P4::Str(s) => s.as_str(),
                other => {
                    return Err(ExecError::MalformedInstruction {
                        opcode: "Function",
                        reason: format!("expected a \"name(arity)\" string P4, got {other:?}"),
                    })
                }
            };
            let (name, arity) = parse_function_descriptor(descriptor).ok_or_else(|| {
                ExecError::MalformedInstruction {
                    opcode: "Function",
                    reason: format!("malformed function descriptor {descriptor:?}"),
                }
            })?;
            let mut args = Vec::with_capacity(arity);
            for i in 0..arity {
                let reg = instr
                    .p2
                    .checked_add(i32::try_from(i).unwrap_or(i32::MAX))
                    .ok_or(ExecError::RegisterOutOfRange {
                        opcode: "Function",
                        index: instr.p2,
                    })?;
                args.push(vm.register(reg)?.clone());
            }
            let result =
                functions::call(name, &args).map_err(|e| ExecError::MalformedInstruction {
                    opcode: "Function",
                    reason: e.to_string(),
                })?;
            vm.set_register(instr.p3, result)?;
            Ok(Step::Next)
        }

        Opcode::AggStep => {
            let (name, arity, collation) = match &instr.p4 {
                P4::AggFunc {
                    name,
                    arity,
                    collation,
                } => (name.as_str(), *arity, *collation),
                other => {
                    return Err(ExecError::MalformedInstruction {
                        opcode: "AggStep",
                        reason: format!("expected an AggFunc P4, got {other:?}"),
                    })
                }
            };
            let mut args = Vec::with_capacity(arity);
            for i in 0..arity {
                let reg = instr
                    .p2
                    .checked_add(i32::try_from(i).unwrap_or(i32::MAX))
                    .ok_or(ExecError::RegisterOutOfRange {
                        opcode: "AggStep",
                        index: instr.p2,
                    })?;
                args.push(vm.register(reg)?.clone());
            }
            let current = if instr.p5 == 0 {
                vm.agg_context(instr.p1)?.cloned()
            } else {
                None
            };
            let updated = aggregate::step(name, current, &args, collation).map_err(|e| {
                ExecError::MalformedInstruction {
                    opcode: "AggStep",
                    reason: e.to_string(),
                }
            })?;
            vm.set_agg_context(instr.p1, updated)?;
            Ok(Step::Next)
        }
        Opcode::AggFinal => {
            let descriptor = match &instr.p4 {
                P4::Str(s) => s.as_str(),
                other => {
                    return Err(ExecError::MalformedInstruction {
                        opcode: "AggFinal",
                        reason: format!("expected a \"name(arity)\" string P4, got {other:?}"),
                    })
                }
            };
            let (name, _arity) = parse_function_descriptor(descriptor).ok_or_else(|| {
                ExecError::MalformedInstruction {
                    opcode: "AggFinal",
                    reason: format!("malformed aggregate descriptor {descriptor:?}"),
                }
            })?;
            let state = vm.agg_context(instr.p1)?;
            let result =
                aggregate::finalize(name, state).map_err(|e| ExecError::MalformedInstruction {
                    opcode: "AggFinal",
                    reason: e.to_string(),
                })?;
            vm.set_register(instr.p3, result)?;
            vm.clear_agg_context(instr.p1)?;
            Ok(Step::Next)
        }

        Opcode::Transaction => {
            if !vm.autocommit {
                return Err(ExecError::TransactionAlreadyActive);
            }
            if let Some(hook) = vm.transaction_hook.as_mut() {
                hook.begin(instr.p1).map_err(ExecError::TransactionFailed)?;
            }
            vm.autocommit = false;
            Ok(Step::Next)
        }
        Opcode::AutoCommit => {
            if vm.autocommit {
                return Err(if instr.p2 == 0 {
                    ExecError::NoActiveTransactionToRollback
                } else {
                    ExecError::NoActiveTransactionToCommit
                });
            }
            if let Some(hook) = vm.transaction_hook.as_mut() {
                if instr.p2 == 0 {
                    hook.rollback().map_err(ExecError::TransactionFailed)?;
                } else {
                    hook.commit().map_err(ExecError::TransactionFailed)?;
                }
            }
            vm.autocommit = true;
            Ok(Step::Next)
        }
        Opcode::SetJournalMode => {
            if !vm.autocommit {
                return Err(ExecError::JournalModeChangeDuringTransaction);
            }
            // The attached pager (via the transaction hook) switches its
            // on-disk journal mode; with no hook this is the no-op
            // sqlite-rs itself falls back to for a read-only connection.
            if let Some(hook) = vm.transaction_hook.as_mut() {
                hook.set_journal_mode(instr.p1)
                    .map_err(ExecError::TransactionFailed)?;
            }
            Ok(Step::Next)
        }
        Opcode::Synchronous => {
            // Query form reports the pager's level (FULL with no pager,
            // sqlite-rs's no-writer fallback); otherwise sets it.
            if instr.p1 == SYNCHRONOUS_QUERY {
                let level = vm
                    .transaction_hook
                    .as_deref()
                    .and_then(|h| h.synchronous())
                    .unwrap_or(SYNCHRONOUS_FULL);
                vm.emit_row(vec![Value::Integer(i64::from(level))]);
                return Ok(Step::Next);
            }
            if let Some(hook) = vm.transaction_hook.as_mut() {
                hook.set_synchronous(instr.p1)
                    .map_err(ExecError::TransactionFailed)?;
            }
            Ok(Step::Next)
        }
        Opcode::IntegrityCheck => {
            // `p1 != 0` is quick_check. One TEXT row per problem line
            // (sqlite-rs `pragma::integrity_check`); without a hook that
            // can read the file this stays Unimplemented.
            let quick = instr.p1 != 0;
            let Some(result) = vm
                .transaction_hook
                .as_mut()
                .and_then(|h| h.integrity_check(quick))
            else {
                return Err(ExecError::Unimplemented {
                    opcode: Opcode::IntegrityCheck,
                });
            };
            for line in result.map_err(ExecError::TransactionFailed)? {
                vm.emit_row(vec![Value::Text(line.into())]);
            }
            Ok(Step::Next)
        }

        Opcode::Delete => {
            let deleted = vm.cursor_mut(instr.p1)?.delete();
            if !deleted {
                return Err(ExecError::MalformedInstruction {
                    opcode: "Delete",
                    reason: "cursor slot does not support deletion".to_string(),
                });
            }
            Ok(Step::Next)
        }
        Opcode::NewRowid => {
            let next = vm.cursor(instr.p1)?.next_rowid();
            // sqlite-rs: p5 != 0 with P4::Str(table) is AUTOINCREMENT —
            // the schema hook consults/updates sqlite_sequence so a rowid
            // is never reused after a delete.
            let rowid = if instr.p5 != 0 {
                let table = match &instr.p4 {
                    P4::Str(name) => name.clone(),
                    other => {
                        return Err(ExecError::MalformedInstruction {
                            opcode: "NewRowid",
                            reason: format!(
                                "AUTOINCREMENT requested (P5 nonzero) but P4 is not a table-name string, got {other:?}"
                            ),
                        })
                    }
                };
                let max_from_table = next.saturating_sub(1);
                vm.schema_storage
                    .as_deref_mut()
                    .ok_or(ExecError::SchemaStorageMissing { opcode: "NewRowid" })?
                    .autoincrement_rowid(&table, max_from_table)
                    .map_err(ExecError::SchemaStorageFailed)?
            } else {
                next
            };
            vm.set_register(instr.p2, Value::Integer(rowid))?;
            Ok(Step::Next)
        }
        Opcode::IdxInsert | Opcode::IdxDelete => {
            let opcode = if instr.opcode == Opcode::IdxInsert {
                "IdxInsert"
            } else {
                "IdxDelete"
            };
            let (key, collations) = key_from_registers(vm, opcode, instr.p2, &instr.p4)?;
            let ok = if instr.opcode == Opcode::IdxInsert {
                // sqlite-rs: on an ephemeral index the stored row is the key
                // columns plus `p5` trailing payload columns.
                let total = key.len().checked_add(usize::from(instr.p5)).ok_or(
                    ExecError::RegisterRangeTooLarge {
                        opcode,
                        count: i32::from(instr.p5),
                    },
                )?;
                let stored = register_range(vm, opcode, instr.p2, total)?;
                let cursor = vm.cursor_mut(instr.p1)?;
                match cursor.ephemeral_idx_insert(&key, &collations, stored) {
                    Some(ok) => ok,
                    None => cursor.idx_insert(key),
                }
            } else {
                vm.cursor_mut(instr.p1)?.idx_delete(&key)
            };
            if !ok {
                return Err(ExecError::MalformedInstruction {
                    opcode,
                    reason:
                        "cursor slot does not support this index operation, or no matching entry"
                            .to_string(),
                });
            }
            Ok(Step::Next)
        }

        Opcode::CreateTable => {
            let P4::CreateTable { name, sql } = &instr.p4 else {
                return Err(ExecError::MalformedInstruction {
                    opcode: "CreateTable",
                    reason: format!("expected a CreateTable P4, got {:?}", instr.p4),
                });
            };
            let (name, sql) = (name.clone(), sql.clone());
            let storage = vm.schema_storage("CreateTable")?;
            let root = storage
                .create_table_root()
                .map_err(ExecError::SchemaStorageFailed)?;
            storage
                .insert_master_row("table", &name, &name, root, &sql)
                .map_err(ExecError::SchemaStorageFailed)?;
            storage
                .bump_schema_cookie()
                .map_err(ExecError::SchemaStorageFailed)?;
            Ok(Step::Next)
        }
        Opcode::CreateView => {
            // sqlite-rs `cursor::create_view`: a view has no b-tree — one
            // sqlite_master row with rootpage 0, then the cookie bump.
            let P4::CreateView { name, sql } = &instr.p4 else {
                return Err(ExecError::MalformedInstruction {
                    opcode: "CreateView",
                    reason: format!("expected a CreateView P4, got {:?}", instr.p4),
                });
            };
            let (name, sql) = (name.clone(), sql.clone());
            let storage = vm.schema_storage("CreateView")?;
            storage
                .insert_master_row("view", &name, &name, 0, &sql)
                .map_err(ExecError::SchemaStorageFailed)?;
            storage
                .bump_schema_cookie()
                .map_err(ExecError::SchemaStorageFailed)?;
            Ok(Step::Next)
        }
        Opcode::CreateIndex => {
            let P4::CreateIndex {
                name,
                table_name,
                table_root_page,
                sql,
                column_indices,
                ..
            } = &instr.p4
            else {
                return Err(ExecError::MalformedInstruction {
                    opcode: "CreateIndex",
                    reason: format!("expected a CreateIndex P4, got {:?}", instr.p4),
                });
            };
            let (name, table_name, table_root_page, sql, column_indices) = (
                name.clone(),
                table_name.clone(),
                *table_root_page,
                sql.clone(),
                column_indices.clone(),
            );
            let storage = vm.schema_storage("CreateIndex")?;
            let root = storage
                .create_index_root()
                .map_err(ExecError::SchemaStorageFailed)?;
            storage
                .populate_index(root, table_root_page, &column_indices)
                .map_err(ExecError::SchemaStorageFailed)?;
            storage
                .insert_master_row("index", &name, &table_name, root, &sql)
                .map_err(ExecError::SchemaStorageFailed)?;
            storage
                .bump_schema_cookie()
                .map_err(ExecError::SchemaStorageFailed)?;
            Ok(Step::Next)
        }
        Opcode::DropTable => {
            let P4::DropTable {
                name,
                root_page,
                indexes,
            } = &instr.p4
            else {
                return Err(ExecError::MalformedInstruction {
                    opcode: "DropTable",
                    reason: format!("expected a DropTable P4, got {:?}", instr.p4),
                });
            };
            let (name, root_page, indexes) = (name.clone(), *root_page, indexes.clone());
            let storage = vm.schema_storage("DropTable")?;
            // sqlite-rs order (page-image parity with the oracle): each
            // index's pages then its row, then the table's pages, then
            // its row.
            for (index_name, index_root) in &indexes {
                storage
                    .free_root(*index_root)
                    .map_err(ExecError::SchemaStorageFailed)?;
                storage
                    .delete_master_row(index_name)
                    .map_err(ExecError::SchemaStorageFailed)?;
            }
            storage
                .free_root(root_page)
                .map_err(ExecError::SchemaStorageFailed)?;
            storage
                .delete_master_row(&name)
                .map_err(ExecError::SchemaStorageFailed)?;
            storage
                .bump_schema_cookie()
                .map_err(ExecError::SchemaStorageFailed)?;
            Ok(Step::Next)
        }
        Opcode::DropIndex => {
            let P4::DropIndex { name, root_page } = &instr.p4 else {
                return Err(ExecError::MalformedInstruction {
                    opcode: "DropIndex",
                    reason: format!("expected a DropIndex P4, got {:?}", instr.p4),
                });
            };
            let (name, root_page) = (name.clone(), *root_page);
            let storage = vm.schema_storage("DropIndex")?;
            storage
                .free_root(root_page)
                .map_err(ExecError::SchemaStorageFailed)?;
            storage
                .delete_master_row(&name)
                .map_err(ExecError::SchemaStorageFailed)?;
            storage
                .bump_schema_cookie()
                .map_err(ExecError::SchemaStorageFailed)?;
            Ok(Step::Next)
        }
        Opcode::Analyze => {
            let P4::Analyze { targets } = &instr.p4 else {
                return Err(ExecError::MalformedInstruction {
                    opcode: "Analyze",
                    reason: format!("expected an Analyze P4, got {:?}", instr.p4),
                });
            };
            let targets = targets.clone();
            let storage = vm.schema_storage("Analyze")?;
            for target in &targets {
                storage
                    .write_stat1(target)
                    .map_err(ExecError::SchemaStorageFailed)?;
            }
            Ok(Step::Next)
        }

        Opcode::IdxRewind => {
            let has_row = vm.cursor_mut(instr.p1)?.rewind();
            Ok(if has_row {
                Step::Next
            } else {
                Step::Jump(to_pc(instr.p2))
            })
        }
        Opcode::IdxLast => {
            let has_row = vm.cursor_mut(instr.p1)?.last();
            Ok(if has_row {
                Step::Next
            } else {
                Step::Jump(to_pc(instr.p2))
            })
        }
        Opcode::IdxNext => {
            let has_row = vm.cursor_mut(instr.p1)?.next();
            Ok(if has_row {
                Step::Jump(to_pc(instr.p2))
            } else {
                Step::Next
            })
        }
        Opcode::IdxPrev => {
            let has_row = vm.cursor_mut(instr.p1)?.prev();
            Ok(if has_row {
                Step::Jump(to_pc(instr.p2))
            } else {
                Step::Next
            })
        }
        Opcode::IdxRowid => {
            let rowid =
                vm.cursor(instr.p1)?
                    .idx_rowid()
                    .ok_or(ExecError::MalformedInstruction {
                        opcode: "IdxRowid",
                        reason: "cursor slot is not an index cursor, or has no current entry"
                            .to_string(),
                    })?;
            vm.set_register(instr.p2, Value::Integer(rowid))?;
            Ok(Step::Next)
        }
        Opcode::SeekIndexEq => {
            let (key, collations) = key_from_registers(vm, "SeekIndexEq", instr.p3, &instr.p4)?;
            let found = vm.cursor_mut(instr.p1)?.seek_index_eq(&key, &collations);
            Ok(if found {
                Step::Next
            } else {
                Step::Jump(to_pc(instr.p2))
            })
        }
        Opcode::SeekIndexGE => {
            let (key, collations) = key_from_registers(vm, "SeekIndexGE", instr.p3, &instr.p4)?;
            let found = vm.cursor_mut(instr.p1)?.seek_index_ge(&key, &collations);
            Ok(if found {
                Step::Next
            } else {
                Step::Jump(to_pc(instr.p2))
            })
        }
        Opcode::IdxCompareGT => {
            let (key, collations) = key_from_registers(vm, "IdxCompareGT", instr.p3, &instr.p4)?;
            let cmp = vm.cursor(instr.p1)?.idx_compare(&key, &collations).ok_or(
                ExecError::MalformedInstruction {
                    opcode: "IdxCompareGT",
                    reason: "cursor slot is not an index cursor, or has no current entry"
                        .to_string(),
                },
            )?;
            Ok(if cmp == Ordering::Greater {
                Step::Jump(to_pc(instr.p2))
            } else {
                Step::Next
            })
        }
        Opcode::IdxLE => {
            let (key, collations) = key_from_registers(vm, "IdxLE", instr.p3, &instr.p4)?;
            let cmp = vm.cursor(instr.p1)?.idx_compare(&key, &collations).ok_or(
                ExecError::MalformedInstruction {
                    opcode: "IdxLE",
                    reason: "cursor slot is not an index cursor, or has no current entry"
                        .to_string(),
                },
            )?;
            Ok(if cmp != Ordering::Greater {
                Step::Jump(to_pc(instr.p2))
            } else {
                Step::Next
            })
        }
        Opcode::Found => {
            let (key, collations) = key_from_registers(vm, "Found", instr.p3, &instr.p4)?;
            let cursor = vm.cursor_mut(instr.p1)?;
            // Ephemeral index (DISTINCT / IN-subquery): membership of the
            // normalized key; real index: a seek.
            let found = match cursor.found(&key, &collations) {
                Some(present) => present,
                None => cursor.seek_index_eq(&key, &collations),
            };
            Ok(if found {
                Step::Jump(to_pc(instr.p2))
            } else {
                Step::Next
            })
        }
        Opcode::NoConflict => {
            let (key, collations) = key_from_registers(vm, "NoConflict", instr.p3, &instr.p4)?;
            let cursor = vm.cursor_mut(instr.p1)?;
            // Ephemeral index (DISTINCT / IN-subquery): membership of the
            // normalized key; real index: a seek.
            let found = match cursor.found(&key, &collations) {
                Some(present) => present,
                None => cursor.seek_index_eq(&key, &collations),
            };
            Ok(if found {
                Step::Next
            } else {
                Step::Jump(to_pc(instr.p2))
            })
        }
    }
}

/// Runs `program` to completion (or the first error/step-limit),
/// returning the rows [`Opcode::ResultRow`] emitted.
pub fn execute(vm: &mut Vm, program: &Program) -> Result<Vec<Vec<Value>>, ExecError> {
    let mut pc = 0usize;
    let mut steps = 0u64;
    loop {
        if pc >= program.instructions.len() {
            return Err(ExecError::ProgramCounterOutOfRange { pc });
        }
        steps = steps.saturating_add(1);
        if steps > MAX_STEPS {
            return Err(ExecError::StepLimitExceeded);
        }
        let instr = &program.instructions[pc];
        match step(vm, pc, instr)? {
            Step::Next => pc = pc.saturating_add(1),
            Step::Jump(target) => pc = target,
            Step::Halt { code: 0, .. } => return Ok(vm.rows().to_vec()),
            Step::Halt { code, message } => return Err(ExecError::Halted { code, message }),
        }
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
    use super::super::cursor::InMemoryCursor;
    use super::super::program::{Instruction, Opcode, Program, P4};
    use super::*;

    fn run(instructions: Vec<Instruction>) -> Vec<Vec<Value>> {
        let mut vm = Vm::new();
        execute(&mut vm, &Program::new(instructions)).unwrap()
    }

    #[test]
    fn goto_jumps_unconditionally() {
        let rows = run(vec![
            Instruction::new(Opcode::Goto, 0, 2, 0),
            Instruction::new(Opcode::Halt, 1, 0, 0), // skipped
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert!(rows.is_empty());
    }

    #[test]
    fn once_falls_through_first_time_then_jumps_on_repeat_entry() {
        // A loop that visits the same `Once` instruction (pc 1) three
        // times: only the first pass runs the guarded `Integer` at pc 2
        // and appends a row; the next two passes jump straight to Halt.
        let mut vm = Vm::new();
        vm.set_register(0, Value::Integer(3)).unwrap();
        let program = Program::new(vec![
            Instruction::new(Opcode::DecrJumpZero, 0, 5, 0), // pc 0: loop counter
            Instruction::new(Opcode::Once, 0, 4, 0),         // pc 1
            Instruction::new(Opcode::Integer, 9, 1, 0),      // pc 2: guarded body
            Instruction::new(Opcode::ResultRow, 1, 1, 0),    // pc 3
            Instruction::new(Opcode::Goto, 0, 0, 0),         // pc 4: back to loop top
            Instruction::new(Opcode::Halt, 0, 0, 0),         // pc 5
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(9)]]);
    }

    #[test]
    fn integer_and_result_row_emit_a_row() {
        let rows = run(vec![
            Instruction::new(Opcode::Integer, 42, 0, 0),
            Instruction::new(Opcode::Integer, 7, 1, 0),
            Instruction::new(Opcode::ResultRow, 0, 2, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Integer(42), Value::Integer(7)]]);
    }

    #[test]
    fn eq_jumps_when_registers_are_equal_skipping_the_fall_through_write() {
        let rows = run(vec![
            Instruction::new(Opcode::Integer, 5, 0, 0),
            Instruction::new(Opcode::Integer, 5, 1, 0),
            Instruction::new(Opcode::Eq, 0, 4, 1), // jump to pc4 (skip pc3) when equal
            Instruction::new(Opcode::Integer, 999, 2, 0), // must be skipped
            Instruction::new(Opcode::ResultRow, 2, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Null]], "reg2 was never written");
    }

    #[test]
    fn eq_falls_through_when_registers_differ() {
        let rows = run(vec![
            Instruction::new(Opcode::Integer, 5, 0, 0),
            Instruction::new(Opcode::Integer, 6, 1, 0),
            Instruction::new(Opcode::Eq, 0, 4, 1),
            Instruction::new(Opcode::Integer, 999, 2, 0),
            Instruction::new(Opcode::ResultRow, 2, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Integer(999)]]);
    }

    #[test]
    fn eq_does_not_jump_on_null_operand() {
        let rows = run(vec![
            Instruction::new(Opcode::Null, 0, 0, 0),
            Instruction::new(Opcode::Integer, 5, 1, 0),
            Instruction::new(Opcode::Eq, 0, 5, 1),
            Instruction::new(Opcode::Integer, 1, 2, 0),
            Instruction::new(Opcode::ResultRow, 2, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Integer(1)]]);
    }

    #[test]
    fn subtract_uses_sqlite_operand_order() {
        // r[p3] = r[p2] - r[p1]
        let rows = run(vec![
            Instruction::new(Opcode::Integer, 3, 0, 0),  // p1
            Instruction::new(Opcode::Integer, 10, 1, 0), // p2
            Instruction::new(Opcode::Subtract, 0, 1, 2),
            Instruction::new(Opcode::ResultRow, 2, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Integer(7)]]);
    }

    #[test]
    fn null_propagates_through_arithmetic() {
        let rows = run(vec![
            Instruction::new(Opcode::Null, 0, 0, 0),
            Instruction::new(Opcode::Integer, 2, 1, 0),
            Instruction::new(Opcode::Add, 0, 1, 2),
            Instruction::new(Opcode::ResultRow, 2, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Null]]);
    }

    #[test]
    fn not_complements_and_propagates_null() {
        let rows = run(vec![
            Instruction::new(Opcode::Integer, 0, 0, 0),
            Instruction::new(Opcode::Not, 0, 1, 0),
            Instruction::new(Opcode::ResultRow, 1, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Integer(1)]]);
    }

    #[test]
    fn if_not_jumps_on_falsy_register() {
        let rows = run(vec![
            Instruction::new(Opcode::Integer, 0, 0, 0),
            Instruction::new(Opcode::IfNot, 0, 4, 0),
            Instruction::new(Opcode::Integer, 1, 1, 0),
            Instruction::new(Opcode::Goto, 0, 5, 0),
            Instruction::new(Opcode::Integer, 2, 1, 0),
            Instruction::new(Opcode::ResultRow, 1, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Integer(2)]]);
    }

    #[test]
    fn decr_jump_zero_terminates_at_zero() {
        let rows = run(vec![
            Instruction::new(Opcode::Integer, 1, 0, 0),
            Instruction::new(Opcode::DecrJumpZero, 0, 3, 0),
            Instruction::new(Opcode::Halt, 1, 0, 0),
            Instruction::new(Opcode::ResultRow, 0, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Integer(0)]]);
    }

    #[test]
    fn cast_forces_target_affinity() {
        let rows = run(vec![
            Instruction::with_p4(Opcode::String8, 0, 0, 0, P4::Str("42".to_string())),
            Instruction::new(
                Opcode::Cast,
                0,
                i32::from(Affinity::Integer.to_p4_byte()),
                0,
            ),
            Instruction::new(Opcode::ResultRow, 0, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Integer(42)]]);
    }

    #[test]
    fn jump_past_the_end_of_the_program_is_an_error() {
        let mut vm = Vm::new();
        let program = Program::new(vec![Instruction::new(Opcode::Goto, 0, 99, 0)]);
        assert!(matches!(
            execute(&mut vm, &program),
            Err(ExecError::ProgramCounterOutOfRange { pc: 99 })
        ));
    }

    #[test]
    fn halt_with_nonzero_code_is_an_error() {
        let mut vm = Vm::new();
        let program = Program::new(vec![Instruction::new(Opcode::Halt, 1, 0, 0)]);
        assert!(matches!(
            execute(&mut vm, &program),
            Err(ExecError::Halted { code: 1, .. })
        ));
    }

    #[test]
    fn cursor_scan_reads_every_row_via_rewind_next_column_rowid() {
        let mut vm = Vm::new();
        vm.open_cursor(
            0,
            Box::new(InMemoryCursor::new(vec![
                vec![Value::Integer(10)],
                vec![Value::Integer(20)],
            ])),
        )
        .unwrap();
        let program = Program::new(vec![
            Instruction::new(Opcode::Rewind, 0, 6, 0),
            Instruction::new(Opcode::Column, 0, 0, 1),
            Instruction::new(Opcode::Rowid, 0, 2, 0),
            Instruction::new(Opcode::ResultRow, 1, 2, 0),
            Instruction::new(Opcode::Next, 0, 1, 0),
            Instruction::new(Opcode::Goto, 0, 6, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(10), Value::Integer(1)],
                vec![Value::Integer(20), Value::Integer(2)],
            ]
        );
    }

    #[test]
    fn seek_rowid_positions_directly_on_a_hit() {
        let mut vm = Vm::new();
        vm.open_cursor(
            0,
            Box::new(InMemoryCursor::new(vec![
                vec![Value::Integer(10)],
                vec![Value::Integer(20)],
                vec![Value::Integer(30)],
            ])),
        )
        .unwrap();
        let program = Program::new(vec![
            Instruction::new(Opcode::Integer, 2, 0, 0),
            Instruction::new(Opcode::SeekRowid, 0, 3, 0),
            Instruction::new(Opcode::Column, 0, 0, 1),
            Instruction::new(Opcode::ResultRow, 1, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(20)]]);
    }

    #[test]
    fn seek_rowid_jumps_to_p2_on_a_miss() {
        let mut vm = Vm::new();
        vm.open_cursor(
            0,
            Box::new(InMemoryCursor::new(vec![vec![Value::Integer(10)]])),
        )
        .unwrap();
        let program = Program::new(vec![
            Instruction::new(Opcode::Integer, 99, 0, 0),
            Instruction::new(Opcode::SeekRowid, 0, 4, 0),
            Instruction::new(Opcode::Column, 0, 0, 1),
            Instruction::new(Opcode::ResultRow, 1, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn unimplemented_opcode_errors_by_name() {
        let mut vm = Vm::new();
        let program = Program::new(vec![Instruction::new(Opcode::IntegrityCheck, 0, 0, 0)]);
        assert!(matches!(
            execute(&mut vm, &program),
            Err(ExecError::Unimplemented {
                opcode: Opcode::IntegrityCheck
            })
        ));
    }

    #[test]
    fn set_journal_mode_is_a_no_op_with_no_writer_attached() {
        let mut vm = Vm::new();
        let instr = Instruction::new(
            Opcode::SetJournalMode,
            super::super::program::JOURNAL_MODE_WAL,
            0,
            0,
        );
        assert_eq!(step(&mut vm, 0, &instr).unwrap(), Step::Next);
    }

    #[test]
    fn set_journal_mode_errors_when_a_transaction_is_open() {
        let mut vm = Vm::new();
        vm.autocommit = false;
        let instr = Instruction::new(
            Opcode::SetJournalMode,
            super::super::program::JOURNAL_MODE_WAL,
            0,
            0,
        );
        assert!(matches!(
            step(&mut vm, 0, &instr),
            Err(ExecError::JournalModeChangeDuringTransaction)
        ));
    }

    #[test]
    fn transaction_with_no_hook_installed_only_toggles_autocommit() {
        let mut vm = Vm::new();
        assert!(vm.autocommit);
        let instr = Instruction::new(
            Opcode::Transaction,
            super::super::program::TRANSACTION_MODE_DEFERRED,
            0,
            0,
        );
        assert_eq!(step(&mut vm, 0, &instr).unwrap(), Step::Next);
        assert!(!vm.autocommit);

        let instr = Instruction::new(Opcode::AutoCommit, 0, 1, 0);
        assert_eq!(step(&mut vm, 0, &instr).unwrap(), Step::Next);
        assert!(vm.autocommit);
    }

    struct RecordingHook {
        calls: Vec<String>,
    }

    impl super::super::transaction::Transaction for RecordingHook {
        fn begin(&mut self, mode: i32) -> Result<(), super::super::transaction::TransactionError> {
            self.calls.push(format!("begin({mode})"));
            Ok(())
        }
        fn commit(&mut self) -> Result<(), super::super::transaction::TransactionError> {
            self.calls.push("commit".to_string());
            Ok(())
        }
        fn rollback(&mut self) -> Result<(), super::super::transaction::TransactionError> {
            self.calls.push("rollback".to_string());
            Ok(())
        }
    }

    #[test]
    fn transaction_and_auto_commit_drive_an_installed_hook() {
        let mut vm = Vm::new();
        vm.set_transaction_hook(Box::new(RecordingHook { calls: Vec::new() }));

        let begin = Instruction::new(
            Opcode::Transaction,
            super::super::program::TRANSACTION_MODE_IMMEDIATE,
            0,
            0,
        );
        step(&mut vm, 0, &begin).unwrap();

        let commit = Instruction::new(Opcode::AutoCommit, 0, 1, 0);
        step(&mut vm, 0, &commit).unwrap();

        // sqlite-rs: a rollback needs an open transaction.
        step(&mut vm, 0, &begin).unwrap();
        let rollback = Instruction::new(Opcode::AutoCommit, 0, 0, 0);
        step(&mut vm, 0, &rollback).unwrap();

        assert!(vm.autocommit);
    }

    struct FailingHook;

    impl super::super::transaction::Transaction for FailingHook {
        fn begin(&mut self, _mode: i32) -> Result<(), super::super::transaction::TransactionError> {
            Err(super::super::transaction::TransactionError(
                "disk full".to_string(),
            ))
        }
        fn commit(&mut self) -> Result<(), super::super::transaction::TransactionError> {
            Ok(())
        }
        fn rollback(&mut self) -> Result<(), super::super::transaction::TransactionError> {
            Ok(())
        }
    }

    #[test]
    fn transaction_propagates_a_failing_hook_and_leaves_autocommit_set() {
        let mut vm = Vm::new();
        vm.set_transaction_hook(Box::new(FailingHook));
        let instr = Instruction::new(
            Opcode::Transaction,
            super::super::program::TRANSACTION_MODE_DEFERRED,
            0,
            0,
        );
        assert!(matches!(
            step(&mut vm, 0, &instr),
            Err(ExecError::TransactionFailed(_))
        ));
        assert!(vm.autocommit);
    }

    #[test]
    fn synchronous_query_reports_full_with_no_writer_attached() {
        let mut vm = Vm::new();
        let instr = Instruction::new(Opcode::Synchronous, SYNCHRONOUS_QUERY, 0, 0);
        assert_eq!(step(&mut vm, 0, &instr).unwrap(), Step::Next);
        assert_eq!(vm.rows(), &[vec![Value::Integer(SYNCHRONOUS_FULL.into())]]);
    }

    #[test]
    fn synchronous_set_is_a_no_op_with_no_writer_attached() {
        let mut vm = Vm::new();
        let instr = Instruction::new(
            Opcode::Synchronous,
            super::super::program::SYNCHRONOUS_OFF,
            0,
            0,
        );
        assert_eq!(step(&mut vm, 0, &instr).unwrap(), Step::Next);
        assert!(vm.rows().is_empty());
    }

    #[test]
    fn make_record_output_matches_expected_encoding() {
        let mut vm = Vm::new();
        vm.set_register(0, Value::Integer(42)).unwrap();
        vm.set_register(1, Value::Text("abc".to_string().into()))
            .unwrap();
        let program = Program::new(vec![
            Instruction::new(Opcode::MakeRecord, 0, 2, 2),
            Instruction::new(Opcode::ResultRow, 2, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        let Value::Blob(payload) = &rows[0][0] else {
            panic!("expected a Blob");
        };
        assert_eq!(&payload[..], &[3, 1, 19, 42, b'a', b'b', b'c']);
    }

    #[test]
    fn make_record_applies_p4_affinity_before_encoding() {
        let mut vm = Vm::new();
        vm.set_register(0, Value::Text("42".to_string().into()))
            .unwrap();
        let program = Program::new(vec![
            Instruction::with_p4(
                Opcode::MakeRecord,
                0,
                1,
                1,
                P4::Affinity(vec![Affinity::Integer.to_p4_byte()]),
            ),
            Instruction::new(Opcode::ResultRow, 1, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        let Value::Blob(payload) = &rows[0][0] else {
            panic!("expected a Blob");
        };
        // serial type 9 (constant 1) never appears for "42"; expect
        // integer serial type 1 (i8), body byte 42 -- proving affinity
        // coerced the text register before encoding, not after.
        assert_eq!(&payload[..], &[2, 1, 42]);
        // The source register is untouched -- affinity applies to a
        // copy, not the live register.
        assert_eq!(
            *vm.register(0).unwrap(),
            Value::Text("42".to_string().into())
        );
    }

    #[test]
    fn ephemeral_table_round_trips_makerecord_insert_scan() {
        // MakeRecord -> Insert -> Rewind/Next -> Column -> ResultRow:
        // the first genuinely complete end-to-end micro-query, entirely
        // storage-agnostic.
        let mut vm = Vm::new();
        let program = Program::new(vec![
            /* 0 */ open_ephemeral_table(0), // cursor 0
            // Row 1: (rowid=1, "a")
            /* 1 */
            Instruction::with_p4(Opcode::String8, 0, 1, 0, P4::Str("a".to_string())),
            /* 2 */
            Instruction::new(Opcode::MakeRecord, 1, 1, 2), // reg2 = record([reg1])
            /* 3 */ Instruction::new(Opcode::Integer, 1, 3, 0), // reg3 = rowid 1
            /* 4 */ Instruction::new(Opcode::Insert, 0, 3, 2),
            // Row 2: (rowid=2, "b")
            /* 5 */
            Instruction::with_p4(Opcode::String8, 0, 1, 0, P4::Str("b".to_string())),
            /* 6 */ Instruction::new(Opcode::MakeRecord, 1, 1, 2),
            /* 7 */ Instruction::new(Opcode::Integer, 2, 3, 0),
            /* 8 */ Instruction::new(Opcode::Insert, 0, 3, 2),
            // Scan cursor 0, emitting (col0, rowid) per row.
            /* 9 */
            Instruction::new(Opcode::Rewind, 0, 14, 0), // jump to Halt(14) if empty
            /* 10 */ Instruction::new(Opcode::Column, 0, 0, 4),
            /* 11 */ Instruction::new(Opcode::Rowid, 0, 5, 0),
            /* 12 */ Instruction::new(Opcode::ResultRow, 4, 2, 0),
            /* 13 */
            Instruction::new(Opcode::Next, 0, 10, 0), // jump to Column(10) if a next row exists
            /* 14 */ Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Value::Text("a".to_string().into()), Value::Integer(1)],
                vec![Value::Text("b".to_string().into()), Value::Integer(2)],
            ]
        );
    }

    #[test]
    fn insert_into_a_non_ephemeral_cursor_slot_errors() {
        let mut vm = Vm::new();
        vm.open_cursor(
            0,
            Box::new(super::super::cursor::InMemoryCursor::new(vec![])),
        )
        .unwrap();
        vm.set_register(1, Value::Integer(1)).unwrap();
        vm.set_register(
            2,
            Value::Blob(encode_record(&[], TextEncoding::Utf8).into()),
        )
        .unwrap();
        let program = Program::new(vec![Instruction::new(Opcode::Insert, 0, 1, 2)]);
        assert!(matches!(
            execute(&mut vm, &program),
            Err(ExecError::MalformedInstruction {
                opcode: "Insert",
                ..
            })
        ));
    }

    #[test]
    fn count_and_sum_over_an_ephemeral_table_scan() {
        // MakeRecord/Insert three rows (10, 20, 30), then AggStep
        // COUNT(*)/SUM(x) over a Rewind/Next scan, AggFinal, ResultRow.
        let mut vm = Vm::new();
        let count_p4 = || P4::AggFunc {
            name: "count".to_string(),
            arity: 0,
            collation: Collation::Binary,
        };
        let sum_p4 = || P4::AggFunc {
            name: "sum".to_string(),
            arity: 1,
            collation: Collation::Binary,
        };
        let program = Program::new(vec![
            /* 0 */ open_ephemeral_table(0),
            // Row 1: (rowid=1, 10)
            /* 1 */
            Instruction::new(Opcode::Integer, 10, 1, 0),
            /* 2 */ Instruction::new(Opcode::MakeRecord, 1, 1, 2),
            /* 3 */ Instruction::new(Opcode::Integer, 1, 3, 0),
            /* 4 */ Instruction::new(Opcode::Insert, 0, 3, 2),
            // Row 2: (rowid=2, 20)
            /* 5 */
            Instruction::new(Opcode::Integer, 20, 1, 0),
            /* 6 */ Instruction::new(Opcode::MakeRecord, 1, 1, 2),
            /* 7 */ Instruction::new(Opcode::Integer, 2, 3, 0),
            /* 8 */ Instruction::new(Opcode::Insert, 0, 3, 2),
            // Row 3: (rowid=3, 30)
            /* 9 */
            Instruction::new(Opcode::Integer, 30, 1, 0),
            /* 10 */ Instruction::new(Opcode::MakeRecord, 1, 1, 2),
            /* 11 */ Instruction::new(Opcode::Integer, 3, 3, 0),
            /* 12 */ Instruction::new(Opcode::Insert, 0, 3, 2),
            // Scan, accumulating COUNT(*) (slot 0) and SUM(col0) (slot 1).
            /* 13 */
            Instruction::new(Opcode::Rewind, 0, 21, 0), // jump to Halt(21) if empty
            /* 14 */ Instruction::new(Opcode::Column, 0, 0, 4),
            /* 15 */ Instruction::with_p4(Opcode::AggStep, 0, 4, 0, count_p4()),
            /* 16 */ Instruction::with_p4(Opcode::AggStep, 1, 4, 0, sum_p4()),
            /* 17 */
            Instruction::new(Opcode::Next, 0, 14, 0), // jump to Column(14) if a next row exists
            /* 18 */
            Instruction::with_p4(Opcode::AggFinal, 0, 0, 5, P4::Str("count(0)".to_string())),
            /* 19 */
            Instruction::with_p4(Opcode::AggFinal, 1, 0, 6, P4::Str("sum(1)".to_string())),
            /* 20 */ Instruction::new(Opcode::ResultRow, 5, 2, 0),
            /* 21 */ Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(3), Value::Integer(60)]]);
    }

    #[test]
    fn group_by_hash_aggregation_sums_per_group_ordered_by_key() {
        // Three rows (group=1,val=10), (group=2,val=5), (group=1,val=20):
        // MakeRecord/HashAggFind/HashAggStep(sum) per row, then
        // HashAggRewind/Data/Next scan out the two groups in key order,
        // AggFinal-ing each group's sum via HashAggData's installed
        // accumulators.
        let mut vm = Vm::new();
        let group_key = P4::GroupKey(vec![super::super::program::GroupKeyColumn {
            index: 0,
            collation: Collation::Binary,
            affinity: b'A',
        }]);
        let sum_p4 = || P4::AggFunc {
            name: "sum".to_string(),
            arity: 1,
            collation: Collation::Binary,
        };
        let program = Program::new(vec![
            /* 0 */ Instruction::with_p4(Opcode::HashAggOpen, 0, 0, 0, group_key),
            // Row 1: group=1, val=10
            /* 1 */
            Instruction::new(Opcode::Integer, 1, 1, 0),
            /* 2 */ Instruction::new(Opcode::Integer, 10, 2, 0),
            /* 3 */ Instruction::new(Opcode::MakeRecord, 1, 2, 3),
            /* 4 */ Instruction::new(Opcode::HashAggFind, 0, 3, 0),
            /* 5 */ Instruction::with_p4(Opcode::HashAggStep, 0, 2, 0, sum_p4()),
            // Row 2: group=2, val=5
            /* 6 */
            Instruction::new(Opcode::Integer, 2, 1, 0),
            /* 7 */ Instruction::new(Opcode::Integer, 5, 2, 0),
            /* 8 */ Instruction::new(Opcode::MakeRecord, 1, 2, 3),
            /* 9 */ Instruction::new(Opcode::HashAggFind, 0, 3, 0),
            /* 10 */ Instruction::with_p4(Opcode::HashAggStep, 0, 2, 0, sum_p4()),
            // Row 3: group=1, val=20
            /* 11 */
            Instruction::new(Opcode::Integer, 1, 1, 0),
            /* 12 */ Instruction::new(Opcode::Integer, 20, 2, 0),
            /* 13 */ Instruction::new(Opcode::MakeRecord, 1, 2, 3),
            /* 14 */ Instruction::new(Opcode::HashAggFind, 0, 3, 0),
            /* 15 */ Instruction::with_p4(Opcode::HashAggStep, 0, 2, 0, sum_p4()),
            // Scan the two groups out in key order.
            /* 16 */
            Instruction::new(Opcode::HashAggRewind, 0, 21, 0), // jump to Halt(21) if empty
            /* 17 */ Instruction::new(Opcode::HashAggData, 0, 4, 0),
            /* 18 */
            Instruction::with_p4(Opcode::AggFinal, 0, 0, 5, P4::Str("sum(1)".to_string())),
            /* 19 */ Instruction::new(Opcode::ResultRow, 4, 2, 0),
            /* 20 */
            Instruction::new(Opcode::HashAggNext, 0, 17, 0), // jump to HashAggData(17) if a next group exists
            /* 21 */ Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows.len(), 2);
        let decoded: Vec<(Value, Value)> = rows
            .into_iter()
            .map(|row| {
                let Value::Blob(bytes) = &row[0] else {
                    panic!("expected a Blob");
                };
                let group = decode_record(bytes, TextEncoding::Utf8).unwrap()[0].clone();
                (group, row[1].clone())
            })
            .collect();
        assert_eq!(
            decoded,
            vec![
                (Value::Integer(1), Value::Integer(30)),
                (Value::Integer(2), Value::Integer(5)),
            ]
        );
    }

    #[test]
    fn agg_final_with_no_agg_step_finalizes_to_the_zero_row_result() {
        let mut vm = Vm::new();
        let program = Program::new(vec![
            Instruction::with_p4(Opcode::AggFinal, 0, 0, 0, P4::Str("count(0)".to_string())),
            Instruction::with_p4(Opcode::AggFinal, 1, 0, 1, P4::Str("sum(1)".to_string())),
            Instruction::new(Opcode::ResultRow, 0, 2, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(0), Value::Null]]);
    }

    #[test]
    fn function_calls_abs_and_upper_end_to_end() {
        let mut vm = Vm::new();
        let program = Program::new(vec![
            Instruction::new(Opcode::Integer, -5, 0, 0), // reg0 = -5
            Instruction::with_p4(Opcode::Function, 0, 0, 1, P4::Str("abs(1)".to_string())), // reg1 = abs(reg0)
            Instruction::with_p4(Opcode::String8, 0, 3, 0, P4::Str("hi".to_string())), // reg3 = "hi"
            Instruction::with_p4(Opcode::Function, 0, 3, 2, P4::Str("upper(1)".to_string())), // reg2 = upper(reg3)
            Instruction::new(Opcode::ResultRow, 1, 2, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(
            rows,
            vec![vec![
                Value::Integer(5),
                Value::Text("HI".to_string().into())
            ]]
        );
    }

    #[test]
    fn function_with_unknown_name_errors() {
        let mut vm = Vm::new();
        let program = Program::new(vec![Instruction::with_p4(
            Opcode::Function,
            0,
            0,
            0,
            P4::Str("median(0)".to_string()),
        )]);
        assert!(matches!(
            execute(&mut vm, &program),
            Err(ExecError::MalformedInstruction {
                opcode: "Function",
                ..
            })
        ));
    }

    #[test]
    fn sorter_scans_makerecord_rows_back_in_sort_order() {
        // MakeRecord/SorterInsert three out-of-order rows, SorterSort,
        // then scan them back via SorterNext/SorterData -- each emitted
        // row is the raw record blob, decoded here the same way
        // sqlite-rs's own sorter test harness does (an OpenPseudo cursor
        // would normally feed Column, which isn't wired for sorters in
        // this minimal, single-key port).
        let mut vm = Vm::new();
        let key = P4::SortKey(vec![super::super::program::SortKeyColumn {
            index: 0,
            descending: false,
            collation: Collation::Binary,
            nulls_first: false,
        }]);
        let program = Program::new(vec![
            /* 0 */ Instruction::with_p4(Opcode::SorterOpen, 0, 0, 0, key),
            /* 1 */ Instruction::new(Opcode::Integer, 30, 1, 0),
            /* 2 */ Instruction::new(Opcode::MakeRecord, 1, 1, 2),
            /* 3 */ Instruction::new(Opcode::SorterInsert, 0, 2, 0),
            /* 4 */ Instruction::new(Opcode::Integer, 10, 1, 0),
            /* 5 */ Instruction::new(Opcode::MakeRecord, 1, 1, 2),
            /* 6 */ Instruction::new(Opcode::SorterInsert, 0, 2, 0),
            /* 7 */ Instruction::new(Opcode::Integer, 20, 1, 0),
            /* 8 */ Instruction::new(Opcode::MakeRecord, 1, 1, 2),
            /* 9 */ Instruction::new(Opcode::SorterInsert, 0, 2, 0),
            /* 10 */
            Instruction::new(Opcode::SorterSort, 0, 14, 0), // jump to Halt(14) if empty
            /* 11 */ Instruction::new(Opcode::SorterData, 0, 3, 0),
            /* 12 */ Instruction::new(Opcode::ResultRow, 3, 1, 0),
            /* 13 */
            Instruction::new(Opcode::SorterNext, 0, 11, 0), // jump to SorterData(11) if a next row exists
            /* 14 */ Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        let decoded: Vec<Value> = rows
            .into_iter()
            .map(|row| {
                let Value::Blob(bytes) = &row[0] else {
                    panic!("expected a Blob");
                };
                decode_record(bytes, TextEncoding::Utf8).unwrap()[0].clone()
            })
            .collect();
        assert_eq!(
            decoded,
            vec![Value::Integer(10), Value::Integer(20), Value::Integer(30)]
        );
    }

    #[test]
    fn sorter_open_reads_bound_from_register_when_p5_nonzero() {
        // SorterOpen(p1=0 cursor, p2=1 bound register, p5=1) with
        // register 1 holding an Integer(2), then insert three rows --
        // only the two smallest should survive.
        let mut vm = Vm::new();
        let key = P4::SortKey(vec![super::super::program::SortKeyColumn {
            index: 0,
            descending: false,
            collation: Collation::Binary,
            nulls_first: false,
        }]);
        let mut instr = Instruction::with_p4(Opcode::SorterOpen, 0, 1, 0, key);
        instr.p5 = 1;
        let program = Program::new(vec![
            /* 0 */ Instruction::new(Opcode::Integer, 2, 1, 0),
            /* 1 */ instr,
            /* 2 */ Instruction::new(Opcode::Integer, 30, 2, 0),
            /* 3 */ Instruction::new(Opcode::MakeRecord, 2, 1, 3),
            /* 4 */ Instruction::new(Opcode::SorterInsert, 0, 3, 0),
            /* 5 */ Instruction::new(Opcode::Integer, 10, 2, 0),
            /* 6 */ Instruction::new(Opcode::MakeRecord, 2, 1, 3),
            /* 7 */ Instruction::new(Opcode::SorterInsert, 0, 3, 0),
            /* 8 */ Instruction::new(Opcode::Integer, 20, 2, 0),
            /* 9 */ Instruction::new(Opcode::MakeRecord, 2, 1, 3),
            /* 10 */ Instruction::new(Opcode::SorterInsert, 0, 3, 0),
            /* 11 */
            Instruction::new(Opcode::SorterSort, 0, 15, 0),
            /* 12 */ Instruction::new(Opcode::SorterData, 0, 4, 0),
            /* 13 */ Instruction::new(Opcode::ResultRow, 4, 1, 0),
            /* 14 */ Instruction::new(Opcode::SorterNext, 0, 12, 0),
            /* 15 */ Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        let decoded: Vec<Value> = rows
            .into_iter()
            .map(|row| {
                let Value::Blob(bytes) = &row[0] else {
                    panic!("expected a Blob");
                };
                decode_record(bytes, TextEncoding::Utf8).unwrap()[0].clone()
            })
            .collect();
        assert_eq!(decoded, vec![Value::Integer(10), Value::Integer(20)]);
    }

    #[test]
    fn sorter_data_before_sort_errors() {
        let mut vm = Vm::new();
        let key = P4::SortKey(vec![super::super::program::SortKeyColumn {
            index: 0,
            descending: false,
            collation: Collation::Binary,
            nulls_first: false,
        }]);
        let program = Program::new(vec![
            Instruction::with_p4(Opcode::SorterOpen, 0, 0, 0, key),
            Instruction::new(Opcode::SorterData, 0, 1, 0),
        ]);
        assert!(matches!(
            execute(&mut vm, &program),
            Err(ExecError::MalformedInstruction {
                opcode: "SorterData",
                ..
            })
        ));
    }

    #[test]
    fn open_read_over_a_prewired_cursor_scans_normally() {
        // Real root-page/pager semantics aren't implemented yet
        // (db-storage wiring is a follow-up) -- OpenRead/OpenWrite just
        // assert the caller already wired the slot via `open_cursor`.
        let mut vm = Vm::new();
        vm.open_cursor(
            0,
            Box::new(super::super::cursor::InMemoryCursor::new(vec![vec![
                Value::Integer(42),
            ]])),
        )
        .unwrap();
        let program = Program::new(vec![
            Instruction::new(Opcode::OpenRead, 0, 0, 0),
            Instruction::new(Opcode::Rewind, 0, 3, 0),
            Instruction::new(Opcode::Column, 0, 0, 1),
            Instruction::new(Opcode::ResultRow, 1, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(42)]]);
    }

    #[test]
    fn open_read_over_an_unwired_slot_errors() {
        let mut vm = Vm::new();
        let program = Program::new(vec![Instruction::new(Opcode::OpenRead, 0, 0, 0)]);
        assert!(matches!(
            execute(&mut vm, &program),
            Err(ExecError::CursorNotOpen { slot: 0 })
        ));
    }

    /// A minimal [`super::super::cursor_factory::CursorFactory`] over a
    /// fixed table of `root -> rows` -- this suite's stand-in for a
    /// real consumer pager.
    struct TestCursorFactory {
        tables: std::collections::HashMap<u32, Vec<Vec<Value>>>,
    }

    impl super::super::cursor_factory::CursorFactory for TestCursorFactory {
        fn open_read(
            &mut self,
            root: u32,
        ) -> Result<Box<dyn Cursor>, super::super::cursor_factory::CursorFactoryError> {
            let rows = self.tables.get(&root).cloned().unwrap_or_default();
            Ok(Box::new(super::super::cursor::InMemoryCursor::new(rows)))
        }
    }

    #[test]
    fn cursor_factory_opens_two_different_roots_by_p2() {
        let mut vm = Vm::new();
        vm.set_cursor_factory(Box::new(TestCursorFactory {
            tables: std::collections::HashMap::from([
                (10, vec![vec![Value::Integer(1)]]),
                (20, vec![vec![Value::Integer(2)]]),
            ]),
        }));
        let program = Program::new(vec![
            Instruction::new(Opcode::OpenRead, 0, 10, 0),
            Instruction::new(Opcode::OpenRead, 1, 20, 0),
            Instruction::new(Opcode::Rewind, 0, 4, 0),
            Instruction::new(Opcode::Column, 0, 0, 2),
            Instruction::new(Opcode::Rewind, 1, 6, 0),
            Instruction::new(Opcode::Column, 1, 0, 3),
            Instruction::new(Opcode::ResultRow, 2, 2, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(2)]]);
    }

    #[test]
    fn open_read_with_nonzero_p3_errors_attached_databases_unsupported() {
        let mut vm = Vm::new();
        let mut instr = Instruction::new(Opcode::OpenRead, 0, 0, 0);
        instr.p3 = 1;
        let program = Program::new(vec![instr]);
        assert!(matches!(
            execute(&mut vm, &program),
            Err(ExecError::AttachedDatabasesUnsupported)
        ));
    }

    #[test]
    fn open_dup_opens_a_second_cursor_onto_the_same_root() {
        let mut vm = Vm::new();
        vm.set_cursor_factory(Box::new(TestCursorFactory {
            tables: std::collections::HashMap::from([(10, vec![vec![Value::Integer(1)]])]),
        }));
        let program = Program::new(vec![
            Instruction::new(Opcode::OpenRead, 0, 10, 0),
            Instruction::new(Opcode::OpenDup, 1, 0, 0),
            Instruction::new(Opcode::Rewind, 1, 4, 0),
            Instruction::new(Opcode::Column, 1, 0, 1),
            Instruction::new(Opcode::ResultRow, 1, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(1)]]);
    }

    #[test]
    fn open_pseudo_reads_columns_from_the_makerecord_blob_in_p2() {
        let mut vm = Vm::new();
        let program = Program::new(vec![
            Instruction::new(Opcode::Integer, 42, 0, 0),
            Instruction::new(Opcode::MakeRecord, 0, 1, 1),
            Instruction::new(Opcode::OpenPseudo, 0, 1, 0),
            Instruction::new(Opcode::Rewind, 0, 6, 0),
            Instruction::new(Opcode::Column, 0, 0, 2),
            Instruction::new(Opcode::ResultRow, 2, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(42)]]);
    }

    /// Records every call it receives, in order, as a plain string log
    /// -- this suite's stand-in for a real consumer's schema storage.
    /// `log` is shared (`Rc<RefCell<_>>`) so a test can still read it
    /// back after the `Box<dyn SchemaStorage>` it's installed behind is
    /// consumed by `Vm`.
    #[derive(Default)]
    struct TestSchemaStorage {
        log: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
        next_root: u32,
    }

    impl super::super::schema_storage::SchemaStorage for TestSchemaStorage {
        fn create_table_root(
            &mut self,
        ) -> Result<u32, super::super::schema_storage::SchemaStorageError> {
            self.next_root += 1;
            self.log
                .borrow_mut()
                .push(format!("create_table_root -> {}", self.next_root));
            Ok(self.next_root)
        }

        fn create_index_root(
            &mut self,
        ) -> Result<u32, super::super::schema_storage::SchemaStorageError> {
            self.next_root += 1;
            self.log
                .borrow_mut()
                .push(format!("create_index_root -> {}", self.next_root));
            Ok(self.next_root)
        }

        fn populate_index(
            &mut self,
            index_root: u32,
            table_root: u32,
            column_indices: &[usize],
        ) -> Result<(), super::super::schema_storage::SchemaStorageError> {
            self.log.borrow_mut().push(format!(
                "populate_index({index_root}, {table_root}, {column_indices:?})"
            ));
            Ok(())
        }

        fn free_root(
            &mut self,
            root: u32,
        ) -> Result<(), super::super::schema_storage::SchemaStorageError> {
            self.log.borrow_mut().push(format!("free_root({root})"));
            Ok(())
        }

        fn insert_master_row(
            &mut self,
            kind: &str,
            name: &str,
            tbl_name: &str,
            root_page: u32,
            sql: &str,
        ) -> Result<(), super::super::schema_storage::SchemaStorageError> {
            self.log.borrow_mut().push(format!(
                "insert_master_row({kind}, {name}, {tbl_name}, {root_page}, {sql})"
            ));
            Ok(())
        }

        fn delete_master_row(
            &mut self,
            name: &str,
        ) -> Result<(), super::super::schema_storage::SchemaStorageError> {
            self.log
                .borrow_mut()
                .push(format!("delete_master_row({name})"));
            Ok(())
        }

        fn bump_schema_cookie(
            &mut self,
        ) -> Result<(), super::super::schema_storage::SchemaStorageError> {
            self.log.borrow_mut().push("bump_schema_cookie".to_string());
            Ok(())
        }

        fn write_stat1(
            &mut self,
            target: &super::super::program::AnalyzeTarget,
        ) -> Result<(), super::super::schema_storage::SchemaStorageError> {
            self.log
                .borrow_mut()
                .push(format!("write_stat1({})", target.table_name));
            Ok(())
        }
    }

    fn instr_p4(opcode: Opcode, p4: P4) -> Instruction {
        let mut instr = Instruction::new(opcode, 0, 0, 0);
        instr.p4 = p4;
        instr
    }

    #[test]
    fn create_table_without_a_schema_storage_hook_errors() {
        let mut vm = Vm::new();
        let program = Program::new(vec![instr_p4(
            Opcode::CreateTable,
            P4::CreateTable {
                name: "t".to_string(),
                sql: "CREATE TABLE t (a)".to_string(),
            },
        )]);
        assert!(matches!(
            execute(&mut vm, &program),
            Err(ExecError::SchemaStorageMissing {
                opcode: "CreateTable"
            })
        ));
    }

    #[test]
    fn create_table_allocates_a_root_and_writes_the_master_row() {
        let mut vm = Vm::new();
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        vm.set_schema_storage(Box::new(TestSchemaStorage {
            log: log.clone(),
            next_root: 0,
        }));
        let program = Program::new(vec![
            instr_p4(
                Opcode::CreateTable,
                P4::CreateTable {
                    name: "t".to_string(),
                    sql: "CREATE TABLE t (a)".to_string(),
                },
            ),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        execute(&mut vm, &program).unwrap();
        assert_eq!(
            *log.borrow(),
            vec![
                "create_table_root -> 1",
                "insert_master_row(table, t, t, 1, CREATE TABLE t (a))",
                "bump_schema_cookie",
            ]
        );
    }

    #[test]
    fn create_view_writes_a_rootless_master_row_and_bumps_the_cookie() {
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut vm = Vm::new();
        vm.set_schema_storage(Box::new(TestSchemaStorage {
            log: std::rc::Rc::clone(&log),
            next_root: 0,
        }));
        let program = Program::new(vec![
            Instruction::with_p4(
                Opcode::CreateView,
                0,
                0,
                0,
                P4::CreateView {
                    name: "v".to_string(),
                    sql: "CREATE VIEW v AS SELECT 1".to_string(),
                },
            ),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        execute(&mut vm, &program).unwrap();
        assert_eq!(
            *log.borrow(),
            vec![
                "insert_master_row(view, v, v, 0, CREATE VIEW v AS SELECT 1)",
                "bump_schema_cookie",
            ]
        );
    }

    #[test]
    fn create_index_populates_from_the_target_table_root() {
        let mut vm = Vm::new();
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        vm.set_schema_storage(Box::new(TestSchemaStorage {
            log: log.clone(),
            next_root: 0,
        }));
        let program = Program::new(vec![
            instr_p4(
                Opcode::CreateIndex,
                P4::CreateIndex {
                    name: "idx".to_string(),
                    table_name: "t".to_string(),
                    table_root_page: 7,
                    sql: "CREATE INDEX idx ON t(a)".to_string(),
                    column_indices: vec![0],
                    unique: false,
                },
            ),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        execute(&mut vm, &program).unwrap();
        assert_eq!(
            *log.borrow(),
            vec![
                "create_index_root -> 1",
                "populate_index(1, 7, [0])",
                "insert_master_row(index, idx, t, 1, CREATE INDEX idx ON t(a))",
                "bump_schema_cookie",
            ]
        );
    }

    #[test]
    fn drop_table_frees_its_own_root_and_every_index_root() {
        let mut vm = Vm::new();
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        vm.set_schema_storage(Box::new(TestSchemaStorage {
            log: log.clone(),
            next_root: 0,
        }));
        let program = Program::new(vec![
            instr_p4(
                Opcode::DropTable,
                P4::DropTable {
                    name: "t".to_string(),
                    root_page: 5,
                    indexes: vec![("idx".to_string(), 6)],
                },
            ),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        execute(&mut vm, &program).unwrap();
        assert_eq!(
            *log.borrow(),
            vec![
                "free_root(6)",
                "delete_master_row(idx)",
                "free_root(5)",
                "delete_master_row(t)",
                "bump_schema_cookie",
            ]
        );
    }

    #[test]
    fn drop_index_frees_its_root() {
        let mut vm = Vm::new();
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        vm.set_schema_storage(Box::new(TestSchemaStorage {
            log: log.clone(),
            next_root: 0,
        }));
        let program = Program::new(vec![
            instr_p4(
                Opcode::DropIndex,
                P4::DropIndex {
                    name: "idx".to_string(),
                    root_page: 6,
                },
            ),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        execute(&mut vm, &program).unwrap();
        assert_eq!(
            *log.borrow(),
            vec![
                "free_root(6)",
                "delete_master_row(idx)",
                "bump_schema_cookie"
            ]
        );
    }

    #[test]
    fn analyze_writes_stat1_for_every_target() {
        use super::super::program::{AnalyzeIndexTarget, AnalyzeTarget};
        let mut vm = Vm::new();
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        vm.set_schema_storage(Box::new(TestSchemaStorage {
            log: log.clone(),
            next_root: 0,
        }));
        let program = Program::new(vec![
            instr_p4(
                Opcode::Analyze,
                P4::Analyze {
                    targets: vec![AnalyzeTarget {
                        table_name: "t".to_string(),
                        table_root_page: 5,
                        indexes: vec![AnalyzeIndexTarget {
                            index_name: "idx".to_string(),
                            root_page: 6,
                        }],
                    }],
                },
            ),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        execute(&mut vm, &program).unwrap();
        assert_eq!(*log.borrow(), vec!["write_stat1(t)"]);
    }

    #[test]
    fn count_falls_back_to_a_scan_when_the_cursor_has_no_fast_count() {
        let mut vm = Vm::new();
        vm.open_cursor(
            0,
            Box::new(InMemoryCursor::new(vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
                vec![Value::Integer(3)],
            ])),
        )
        .unwrap();
        let program = Program::new(vec![
            Instruction::new(Opcode::Count, 0, 1, 0),
            Instruction::new(Opcode::ResultRow, 1, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(3)]]);
    }

    #[test]
    fn last_positions_at_the_final_row_jumping_to_p2_when_empty() {
        let mut vm = Vm::new();
        vm.open_cursor(0, Box::new(InMemoryCursor::new(vec![])))
            .unwrap();
        let program = Program::new(vec![
            Instruction::new(Opcode::Last, 0, 3, 0),
            Instruction::new(Opcode::Integer, 1, 0, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
            Instruction::new(Opcode::Integer, 0, 0, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, Vec::<Vec<Value>>::new());
    }

    #[test]
    fn null_row_makes_column_and_rowid_read_as_null_and_zero() {
        let mut vm = Vm::new();
        vm.open_cursor(
            0,
            Box::new(InMemoryCursor::new(vec![vec![Value::Integer(1)]])),
        )
        .unwrap();
        let program = Program::new(vec![
            Instruction::new(Opcode::NullRow, 0, 0, 0),
            Instruction::new(Opcode::Column, 0, 0, 1),
            Instruction::new(Opcode::Rowid, 0, 2, 0),
            Instruction::new(Opcode::ResultRow, 1, 2, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Null, Value::Integer(0)]]);
    }

    #[test]
    fn null_row_flag_clears_on_the_next_rewind() {
        let mut vm = Vm::new();
        vm.open_cursor(
            0,
            Box::new(InMemoryCursor::new(vec![vec![Value::Integer(7)]])),
        )
        .unwrap();
        let program = Program::new(vec![
            Instruction::new(Opcode::NullRow, 0, 0, 0),
            Instruction::new(Opcode::Rewind, 0, 5, 0),
            Instruction::new(Opcode::Column, 0, 0, 1),
            Instruction::new(Opcode::ResultRow, 1, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(7)]]);
    }

    #[test]
    fn sequence_hands_out_increasing_values_per_slot() {
        let mut vm = Vm::new();
        let program = Program::new(vec![
            Instruction::new(Opcode::Sequence, 0, 0, 0),
            Instruction::new(Opcode::Sequence, 0, 1, 0),
            Instruction::new(Opcode::ResultRow, 0, 2, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(0), Value::Integer(1)]]);
    }

    #[test]
    fn auto_index_insert_opens_the_slot_on_first_use_and_seek_finds_it() {
        let mut vm = Vm::new();
        let program = Program::new(vec![
            Instruction::new(Opcode::Integer, 42, 1, 0),
            Instruction::new(Opcode::Integer, 99, 2, 0),
            Instruction::with_p4(Opcode::AutoIndexInsert, 0, 1, 2, P4::Int(1)),
            Instruction::with_p4(Opcode::AutoIndexSeek, 0, 6, 1, P4::Int(1)),
            Instruction::new(Opcode::AutoIndexRowid, 0, 3, 0),
            Instruction::new(Opcode::ResultRow, 3, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(99)]]);
    }

    #[test]
    fn auto_index_seek_jumps_to_p2_on_a_miss() {
        let mut vm = Vm::new();
        let program = Program::new(vec![
            Instruction::new(Opcode::Integer, 42, 1, 0),
            Instruction::new(Opcode::Integer, 99, 2, 0),
            Instruction::with_p4(Opcode::AutoIndexInsert, 0, 1, 2, P4::Int(1)),
            Instruction::new(Opcode::Integer, 7, 1, 0),
            Instruction::with_p4(Opcode::AutoIndexSeek, 0, 7, 1, P4::Int(1)),
            Instruction::new(Opcode::Integer, 1, 0, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
            Instruction::new(Opcode::Integer, 0, 0, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        execute(&mut vm, &program).unwrap();
    }

    fn asc_key(index: usize) -> super::super::program::SortKeyColumn {
        super::super::program::SortKeyColumn {
            index,
            descending: false,
            collation: Collation::Binary,
            nulls_first: false,
        }
    }

    fn open_index_cursor(vm: &mut Vm, slot: i32, rows: &[(i64, Value)]) {
        let mut cursor = super::super::cursor::InMemoryIndexCursor::new(vec![asc_key(0)]);
        for (rowid, value) in rows {
            cursor.insert(*rowid, vec![value.clone()]);
        }
        vm.open_cursor(slot, Box::new(cursor)).unwrap();
    }

    #[test]
    fn idx_rewind_last_next_prev_and_rowid_walk_an_index_cursor() {
        let mut vm = Vm::new();
        open_index_cursor(
            &mut vm,
            0,
            &[(10, Value::Integer(1)), (20, Value::Integer(2))],
        );
        let program = Program::new(vec![
            Instruction::new(Opcode::IdxRewind, 0, 8, 0),
            Instruction::new(Opcode::IdxRowid, 0, 1, 0),
            Instruction::new(Opcode::IdxNext, 0, 4, 0),
            Instruction::new(Opcode::Goto, 0, 8, 0),
            Instruction::new(Opcode::IdxRowid, 0, 2, 0),
            Instruction::new(Opcode::ResultRow, 1, 2, 0),
            Instruction::new(Opcode::Goto, 0, 8, 0),
            Instruction::new(Opcode::Goto, 0, 8, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(10), Value::Integer(20)]]);
    }

    #[test]
    fn idx_last_and_prev_walk_backward() {
        let mut vm = Vm::new();
        open_index_cursor(
            &mut vm,
            0,
            &[(10, Value::Integer(1)), (20, Value::Integer(2))],
        );
        let program = Program::new(vec![
            Instruction::new(Opcode::IdxLast, 0, 6, 0),
            Instruction::new(Opcode::IdxRowid, 0, 1, 0),
            Instruction::new(Opcode::IdxPrev, 0, 4, 0),
            Instruction::new(Opcode::Goto, 0, 6, 0),
            Instruction::new(Opcode::IdxRowid, 0, 2, 0),
            Instruction::new(Opcode::ResultRow, 1, 2, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(20), Value::Integer(10)]]);
    }

    #[test]
    fn seek_index_eq_jumps_to_p2_on_a_miss() {
        let mut vm = Vm::new();
        open_index_cursor(&mut vm, 0, &[(10, Value::Integer(1))]);
        let mut instr = Instruction::new(Opcode::SeekIndexEq, 0, 4, 1);
        instr.p4 = P4::Int(1);
        let program = Program::new(vec![
            Instruction::new(Opcode::Integer, 99, 1, 0),
            instr,
            Instruction::new(Opcode::Integer, 1, 0, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
            Instruction::new(Opcode::Integer, 0, 0, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, Vec::<Vec<Value>>::new());
    }

    #[test]
    fn seek_index_eq_falls_through_on_a_hit() {
        let mut vm = Vm::new();
        open_index_cursor(&mut vm, 0, &[(10, Value::Integer(1))]);
        let mut instr = Instruction::new(Opcode::SeekIndexEq, 0, 5, 1);
        instr.p4 = P4::Int(1);
        let program = Program::new(vec![
            Instruction::new(Opcode::Integer, 1, 1, 0),
            instr,
            Instruction::new(Opcode::IdxRowid, 0, 2, 0),
            Instruction::new(Opcode::ResultRow, 2, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(10)]]);
    }

    #[test]
    fn seek_index_ge_jumps_to_p2_when_none_qualify() {
        let mut vm = Vm::new();
        open_index_cursor(&mut vm, 0, &[(10, Value::Integer(1))]);
        let mut instr = Instruction::new(Opcode::SeekIndexGE, 0, 4, 1);
        instr.p4 = P4::Int(1);
        let program = Program::new(vec![
            Instruction::new(Opcode::Integer, 99, 1, 0),
            instr,
            Instruction::new(Opcode::Integer, 1, 0, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
            Instruction::new(Opcode::Integer, 0, 0, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, Vec::<Vec<Value>>::new());
    }

    #[test]
    fn idx_compare_gt_jumps_when_the_current_entry_exceeds_the_key() {
        let mut vm = Vm::new();
        open_index_cursor(&mut vm, 0, &[(10, Value::Integer(5))]);
        let mut gt = Instruction::new(Opcode::IdxCompareGT, 0, 5, 1);
        gt.p4 = P4::Int(1);
        let program = Program::new(vec![
            Instruction::new(Opcode::IdxRewind, 0, 6, 0),
            Instruction::new(Opcode::Integer, 1, 1, 0),
            gt,
            Instruction::new(Opcode::Integer, 0, 0, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
            Instruction::new(Opcode::Integer, 1, 0, 0),
            Instruction::new(Opcode::ResultRow, 0, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(1)]]);
    }

    #[test]
    fn idx_le_jumps_when_the_current_entry_does_not_exceed_the_key() {
        let mut vm = Vm::new();
        open_index_cursor(&mut vm, 0, &[(10, Value::Integer(5))]);
        let mut le = Instruction::new(Opcode::IdxLE, 0, 5, 1);
        le.p4 = P4::Int(1);
        let program = Program::new(vec![
            Instruction::new(Opcode::IdxRewind, 0, 6, 0),
            Instruction::new(Opcode::Integer, 9, 1, 0),
            le,
            Instruction::new(Opcode::Integer, 0, 0, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
            Instruction::new(Opcode::Integer, 1, 0, 0),
            Instruction::new(Opcode::ResultRow, 0, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(1)]]);
    }

    #[test]
    fn found_jumps_to_p2_on_a_hit_and_no_conflict_jumps_to_p2_on_a_miss() {
        let mut vm = Vm::new();
        open_index_cursor(&mut vm, 0, &[(10, Value::Integer(1))]);
        let mut found = Instruction::new(Opcode::Found, 0, 3, 1);
        found.p4 = P4::Int(1);
        let mut no_conflict = Instruction::new(Opcode::NoConflict, 0, 6, 1);
        no_conflict.p4 = P4::Int(1);
        let program = Program::new(vec![
            Instruction::new(Opcode::Integer, 1, 1, 0),
            found,
            Instruction::new(Opcode::Halt, 0, 0, 0),
            Instruction::new(Opcode::Integer, 99, 1, 0),
            no_conflict,
            Instruction::new(Opcode::Halt, 0, 0, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        execute(&mut vm, &program).unwrap();
    }

    /// `OpenEphemeral` with `p5 = 1`: sqlite-rs's ephemeral *table* (rows by
    /// rowid); `p5 = 0` is the ephemeral index (#134).
    fn open_ephemeral_table(slot: i32) -> Instruction {
        let mut i = Instruction::new(Opcode::OpenEphemeral, slot, 0, 0);
        i.p5 = 1;
        i
    }

    #[test]
    fn ephemeral_index_found_idx_insert_delete_and_column_follow_sqlite_rs() {
        // DISTINCT dance: Found misses, IdxInsert (1 key + 1 payload col),
        // Found hits, Column reads the stored payload, Delete removes it.
        let mut vm = Vm::new();
        let mut idx_insert = Instruction::with_p4(Opcode::IdxInsert, 0, 1, 0, P4::Int(1));
        idx_insert.p5 = 1;
        let program = Program::new(vec![
            /* 0 */ Instruction::new(Opcode::OpenEphemeral, 0, 0, 0),
            /* 1 */ Instruction::new(Opcode::Integer, 7, 1, 0),
            /* 2 */ Instruction::new(Opcode::Integer, 70, 2, 0),
            /* 3 */
            Instruction::with_p4(Opcode::Found, 0, 99, 1, P4::Int(1)), // miss: fall through
            /* 4 */ idx_insert,
            /* 5 */
            Instruction::with_p4(Opcode::Found, 0, 7, 1, P4::Int(1)), // hit: jump to 7
            /* 6 */ Instruction::new(Opcode::Halt, 1, 0, 0),
            /* 7 */ Instruction::new(Opcode::Column, 0, 1, 3), // payload col -> r3
            /* 8 */ Instruction::new(Opcode::Delete, 0, 0, 0),
            /* 9 */
            Instruction::with_p4(Opcode::Found, 0, 99, 1, P4::Int(1)), // gone: fall through
            /* 10 */ Instruction::new(Opcode::ResultRow, 3, 1, 0),
            /* 11 */ Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(70)]]);
    }

    #[test]
    fn seek_key_collations_normalize_ephemeral_index_keys() {
        // NOCASE: 'Abc' and 'ABC' are the same key.
        let mut vm = Vm::new();
        let program = Program::new(vec![
            Instruction::new(Opcode::OpenEphemeral, 0, 0, 0),
            Instruction::with_p4(Opcode::String8, 0, 1, 0, P4::Str("Abc".to_string())),
            Instruction::with_p4(
                Opcode::IdxInsert,
                0,
                1,
                0,
                P4::SeekKey(vec![Collation::NoCase]),
            ),
            Instruction::with_p4(Opcode::String8, 0, 1, 0, P4::Str("ABC".to_string())),
            Instruction::with_p4(Opcode::Found, 0, 6, 1, P4::SeekKey(vec![Collation::NoCase])),
            Instruction::new(Opcode::Halt, 1, 0, 0),
            Instruction::new(Opcode::Integer, 1, 2, 0),
            Instruction::new(Opcode::ResultRow, 2, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(1)]]);
    }

    #[test]
    fn open_dup_shares_an_ephemeral_tables_rows_with_a_fresh_position() {
        let mut vm = Vm::new();
        let program = Program::new(vec![
            open_ephemeral_table(0),
            Instruction::new(Opcode::Integer, 5, 1, 0),
            Instruction::with_p4(Opcode::MakeRecord, 1, 1, 2, P4::None),
            Instruction::new(Opcode::Integer, 1, 3, 0),
            Instruction::new(Opcode::Insert, 0, 3, 2),
            Instruction::new(Opcode::OpenDup, 1, 0, 0),
            Instruction::new(Opcode::Rewind, 1, 9, 0),
            Instruction::new(Opcode::Column, 1, 0, 4),
            Instruction::new(Opcode::ResultRow, 4, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(5)]]);
    }

    #[test]
    fn sequence_starts_at_one_for_ephemeral_tables_and_zero_for_indexes() {
        let mut vm = Vm::new();
        let program = Program::new(vec![
            open_ephemeral_table(0),
            Instruction::new(Opcode::OpenEphemeral, 1, 0, 0),
            Instruction::new(Opcode::Sequence, 0, 1, 0),
            Instruction::new(Opcode::Sequence, 0, 2, 0),
            Instruction::new(Opcode::Sequence, 1, 3, 0),
            Instruction::new(Opcode::ResultRow, 1, 3, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(
            rows,
            vec![vec![
                Value::Integer(1),
                Value::Integer(2),
                Value::Integer(0)
            ]]
        );
    }

    #[test]
    fn open_read_with_p5_flag_asks_the_factory_for_an_index_cursor() {
        struct Factory;
        impl super::super::cursor_factory::CursorFactory for Factory {
            fn open_read(
                &mut self,
                _root: u32,
            ) -> Result<Box<dyn Cursor>, super::super::cursor_factory::CursorFactoryError>
            {
                panic!("p5 = 1 must route to open_index");
            }
            fn open_index(
                &mut self,
                root: u32,
                key: &[super::super::program::SortKeyColumn],
            ) -> Result<Box<dyn Cursor>, super::super::cursor_factory::CursorFactoryError>
            {
                assert_eq!(root, 3);
                assert!(key.is_empty());
                Ok(Box::new(super::super::cursor::InMemoryCursor::new(vec![])))
            }
        }
        let mut vm = Vm::new();
        vm.set_cursor_factory(Box::new(Factory));
        let mut open = Instruction::new(Opcode::OpenRead, 0, 3, 0);
        open.p5 = 1;
        let program = Program::new(vec![open, Instruction::new(Opcode::Halt, 0, 0, 0)]);
        execute(&mut vm, &program).unwrap();
    }

    #[test]
    fn insert_payload_hands_a_storage_cursor_the_exact_record_bytes() {
        use std::rc::Rc;
        type Seen = Rc<std::cell::RefCell<Option<(i64, Rc<[u8]>)>>>;
        struct Sink(Seen);
        impl Cursor for Sink {
            fn rewind(&mut self) -> bool {
                false
            }
            fn next(&mut self) -> bool {
                false
            }
            fn column(&self, _: usize) -> Value {
                Value::Null
            }
            fn rowid(&self) -> i64 {
                0
            }
            fn insert_payload(&mut self, rowid: i64, payload: &Rc<[u8]>) -> Option<bool> {
                *self.0.borrow_mut() = Some((rowid, Rc::clone(payload)));
                Some(true)
            }
        }
        let seen = Rc::new(std::cell::RefCell::new(None));
        let mut vm = Vm::new();
        vm.open_cursor(0, Box::new(Sink(Rc::clone(&seen)))).unwrap();
        let program = Program::new(vec![
            Instruction::new(Opcode::Integer, 5, 1, 0),
            Instruction::with_p4(Opcode::MakeRecord, 1, 1, 2, P4::None),
            Instruction::new(Opcode::Integer, 9, 3, 0),
            Instruction::new(Opcode::Insert, 0, 3, 2),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        execute(&mut vm, &program).unwrap();
        let (rowid, payload) = seen.borrow().clone().expect("insert_payload called");
        assert_eq!(rowid, 9);
        assert_eq!(
            &*payload,
            &super::super::record::encode_record(&[Value::Integer(5)], TextEncoding::Utf8)[..]
        );
    }

    #[test]
    fn new_rowid_with_p5_uses_the_autoincrement_hook() {
        struct Seq;
        impl super::super::schema_storage::SchemaStorage for Seq {
            fn create_table_root(
                &mut self,
            ) -> Result<u32, super::super::schema_storage::SchemaStorageError> {
                unreachable!()
            }
            fn create_index_root(
                &mut self,
            ) -> Result<u32, super::super::schema_storage::SchemaStorageError> {
                unreachable!()
            }
            fn populate_index(
                &mut self,
                _: u32,
                _: u32,
                _: &[usize],
            ) -> Result<(), super::super::schema_storage::SchemaStorageError> {
                unreachable!()
            }
            fn free_root(
                &mut self,
                _: u32,
            ) -> Result<(), super::super::schema_storage::SchemaStorageError> {
                unreachable!()
            }
            fn insert_master_row(
                &mut self,
                _: &str,
                _: &str,
                _: &str,
                _: u32,
                _: &str,
            ) -> Result<(), super::super::schema_storage::SchemaStorageError> {
                unreachable!()
            }
            fn delete_master_row(
                &mut self,
                _: &str,
            ) -> Result<(), super::super::schema_storage::SchemaStorageError> {
                unreachable!()
            }
            fn bump_schema_cookie(
                &mut self,
            ) -> Result<(), super::super::schema_storage::SchemaStorageError> {
                unreachable!()
            }
            fn write_stat1(
                &mut self,
                _: &super::super::program::AnalyzeTarget,
            ) -> Result<(), super::super::schema_storage::SchemaStorageError> {
                unreachable!()
            }
            fn autoincrement_rowid(
                &mut self,
                table: &str,
                max_from_table: i64,
            ) -> Result<i64, super::super::schema_storage::SchemaStorageError> {
                assert_eq!(table, "t");
                // sqlite_sequence remembers 10 even though the table is empty.
                Ok(max_from_table.max(10) + 1)
            }
        }
        let mut vm = Vm::new();
        vm.set_schema_storage(Box::new(Seq));
        let mut new_rowid =
            Instruction::with_p4(Opcode::NewRowid, 0, 1, 0, P4::Str("t".to_string()));
        new_rowid.p5 = 1;
        let program = Program::new(vec![
            open_ephemeral_table(0),
            new_rowid,
            Instruction::new(Opcode::ResultRow, 1, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(11)]]);
    }

    #[test]
    fn nested_begin_and_bare_commit_or_rollback_error_like_sqlite_rs() {
        let mut vm = Vm::new();
        let begin = Instruction::new(
            Opcode::Transaction,
            super::super::program::TRANSACTION_MODE_DEFERRED,
            0,
            0,
        );
        assert!(matches!(
            step(&mut vm, 0, &Instruction::new(Opcode::AutoCommit, 0, 1, 0)),
            Err(ExecError::NoActiveTransactionToCommit)
        ));
        assert!(matches!(
            step(&mut vm, 0, &Instruction::new(Opcode::AutoCommit, 0, 0, 0)),
            Err(ExecError::NoActiveTransactionToRollback)
        ));
        step(&mut vm, 0, &begin).unwrap();
        assert!(matches!(
            step(&mut vm, 0, &begin),
            Err(ExecError::TransactionAlreadyActive)
        ));
    }

    #[test]
    fn journal_mode_synchronous_and_integrity_check_reach_the_hook() {
        use super::super::program::{JOURNAL_MODE_WAL, SYNCHRONOUS_OFF};
        struct PagerHook(Vec<String>, i32);
        impl super::super::transaction::Transaction for PagerHook {
            fn begin(&mut self, _: i32) -> Result<(), super::super::transaction::TransactionError> {
                Ok(())
            }
            fn commit(&mut self) -> Result<(), super::super::transaction::TransactionError> {
                Ok(())
            }
            fn rollback(&mut self) -> Result<(), super::super::transaction::TransactionError> {
                Ok(())
            }
            fn set_journal_mode(
                &mut self,
                mode: i32,
            ) -> Result<(), super::super::transaction::TransactionError> {
                self.0.push(format!("journal({mode})"));
                Ok(())
            }
            fn synchronous(&self) -> Option<i32> {
                Some(self.1)
            }
            fn set_synchronous(
                &mut self,
                level: i32,
            ) -> Result<(), super::super::transaction::TransactionError> {
                self.1 = level;
                Ok(())
            }
            fn integrity_check(
                &mut self,
                quick: bool,
            ) -> Option<Result<Vec<String>, super::super::transaction::TransactionError>>
            {
                Some(Ok(vec![format!("ok quick={quick}")]))
            }
        }
        let mut vm = Vm::new();
        vm.set_transaction_hook(Box::new(PagerHook(Vec::new(), SYNCHRONOUS_FULL)));
        let program = Program::new(vec![
            Instruction::new(Opcode::SetJournalMode, JOURNAL_MODE_WAL, 0, 0),
            Instruction::new(Opcode::Synchronous, SYNCHRONOUS_OFF, 0, 0),
            Instruction::new(Opcode::Synchronous, SYNCHRONOUS_QUERY, 0, 0),
            Instruction::new(Opcode::IntegrityCheck, 1, 0, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(i64::from(SYNCHRONOUS_OFF))],
                vec![Value::Text("ok quick=true".into())],
            ]
        );
    }

    #[test]
    fn opcode_all_is_sqlite_rs_harvested_inventory() {
        assert_eq!(Opcode::ALL.len(), 68);
        assert!(Opcode::ALL.contains(&Opcode::SorterOpen));
        assert!(!Opcode::ALL.contains(&Opcode::AutoCommit));
    }

    /// MC/DC vector (obligation `vm_442`, `Vm::index`'s decision `reg <
    /// 0 || reg as usize > MAX_REGISTERS`): leaf A (`reg < 0`) true.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_442__v1_negative_register_is_out_of_range() {
        let vm = Vm::new();
        assert!(matches!(
            vm.register(-1),
            Err(ExecError::RegisterOutOfRange { index: -1, .. })
        ));
    }

    /// MC/DC vector (obligation `vm_442`): both leaves false -- an
    /// ordinary in-range register. Independence pair for A against
    /// `mcdc__vm_442__v1_negative_register_is_out_of_range`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_442__v2_in_range_register_is_ok() {
        let vm = Vm::new();
        assert!(vm.register(0).is_ok());
    }

    /// MC/DC vector (obligation `vm_442`): leaf B (`reg as usize >
    /// MAX_REGISTERS`) true, leaf A false. Independence pair for B
    /// against `mcdc__vm_442__v2_in_range_register_is_ok`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_442__v3_register_past_the_cap_is_out_of_range() {
        let vm = Vm::new();
        let past_cap = i32::try_from(MAX_REGISTERS).unwrap().saturating_add(1);
        assert!(matches!(
            vm.register(past_cap),
            Err(ExecError::RegisterOutOfRange { .. })
        ));
    }

    /// MC/DC vector (obligation `vm_579`, `compare_jump`'s decision
    /// `matches!(a, Value::Null) || matches!(b, Value::Null)`): leaf A
    /// (`a` is NULL) true -- no jump is taken regardless of `b`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_579__v1_lhs_null_suppresses_the_jump() {
        let rows = run(vec![
            Instruction::new(Opcode::Null, 0, 0, 0),
            Instruction::new(Opcode::Integer, 5, 1, 0),
            Instruction::new(Opcode::Eq, 0, 4, 1),
            Instruction::new(Opcode::Integer, 1, 2, 0),
            Instruction::new(Opcode::ResultRow, 2, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Integer(1)]], "no jump was taken");
    }

    /// MC/DC vector (obligation `vm_579`): both leaves false -- neither
    /// operand is NULL, so the comparison runs normally and the jump is
    /// taken on equality. Independence pair for A against
    /// `mcdc__vm_579__v1_lhs_null_suppresses_the_jump`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_579__v2_neither_null_lets_the_comparison_decide() {
        let rows = run(vec![
            Instruction::new(Opcode::Integer, 5, 0, 0),
            Instruction::new(Opcode::Integer, 5, 1, 0),
            Instruction::new(Opcode::Eq, 0, 4, 1),
            Instruction::new(Opcode::Integer, 999, 2, 0),
            Instruction::new(Opcode::ResultRow, 2, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Null]], "the jump was taken");
    }

    /// MC/DC vector (obligation `vm_579`): leaf B (`b` is NULL) true,
    /// leaf A false -- no jump. Independence pair for B against
    /// `mcdc__vm_579__v2_neither_null_lets_the_comparison_decide`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_579__v3_rhs_null_suppresses_the_jump() {
        let rows = run(vec![
            Instruction::new(Opcode::Integer, 5, 0, 0),
            Instruction::new(Opcode::Null, 0, 1, 0),
            Instruction::new(Opcode::Eq, 0, 4, 1),
            Instruction::new(Opcode::Integer, 1, 2, 0),
            Instruction::new(Opcode::ResultRow, 2, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Integer(1)]], "no jump was taken");
    }

    /// MC/DC vector (obligation `vm_628`, `binary_op`'s decision
    /// `matches!(a, Value::Null) || matches!(b, Value::Null)`): leaf A
    /// true -- the result is NULL regardless of `b`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_628__v1_lhs_null_forces_null_result() {
        let rows = run(vec![
            Instruction::new(Opcode::Null, 0, 0, 0),
            Instruction::new(Opcode::Integer, 5, 1, 0),
            Instruction::new(Opcode::Add, 0, 1, 2),
            Instruction::new(Opcode::ResultRow, 2, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Null]]);
    }

    /// MC/DC vector (obligation `vm_628`): both leaves false -- the
    /// underlying operation actually runs. Independence pair for A
    /// against `mcdc__vm_628__v1_lhs_null_forces_null_result`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_628__v2_neither_null_runs_the_operation() {
        let rows = run(vec![
            Instruction::new(Opcode::Integer, 2, 0, 0),
            Instruction::new(Opcode::Integer, 3, 1, 0),
            Instruction::new(Opcode::Add, 0, 1, 2),
            Instruction::new(Opcode::ResultRow, 2, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Integer(5)]]);
    }

    /// MC/DC vector (obligation `vm_628`): leaf B true, leaf A false --
    /// the result is NULL. Independence pair for B against
    /// `mcdc__vm_628__v2_neither_null_runs_the_operation`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_628__v3_rhs_null_forces_null_result() {
        let rows = run(vec![
            Instruction::new(Opcode::Integer, 5, 0, 0),
            Instruction::new(Opcode::Null, 0, 1, 0),
            Instruction::new(Opcode::Add, 0, 1, 2),
            Instruction::new(Opcode::ResultRow, 2, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Null]]);
    }

    /// MC/DC vector (obligation `vm_648`, `binary_op_reversed`'s
    /// decision `matches!(a, Value::Null) || matches!(b, Value::Null)`):
    /// leaf A (`p1`'s operand) true -- the result is NULL.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_648__v1_lhs_null_forces_null_result() {
        let rows = run(vec![
            Instruction::new(Opcode::Null, 0, 0, 0),
            Instruction::new(Opcode::Integer, 10, 1, 0),
            Instruction::new(Opcode::Subtract, 0, 1, 2),
            Instruction::new(Opcode::ResultRow, 2, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Null]]);
    }

    /// MC/DC vector (obligation `vm_648`): both leaves false -- the
    /// reversed subtraction (`p2 - p1`) actually runs. Independence pair
    /// for A against `mcdc__vm_648__v1_lhs_null_forces_null_result`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_648__v2_neither_null_runs_the_operation() {
        let rows = run(vec![
            Instruction::new(Opcode::Integer, 3, 0, 0),
            Instruction::new(Opcode::Integer, 10, 1, 0),
            Instruction::new(Opcode::Subtract, 0, 1, 2),
            Instruction::new(Opcode::ResultRow, 2, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Integer(7)]]);
    }

    /// MC/DC vector (obligation `vm_648`): leaf B (`p2`'s operand) true,
    /// leaf A false -- the result is NULL. Independence pair for B
    /// against `mcdc__vm_648__v2_neither_null_runs_the_operation`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_648__v3_rhs_null_forces_null_result() {
        let rows = run(vec![
            Instruction::new(Opcode::Integer, 3, 0, 0),
            Instruction::new(Opcode::Null, 0, 1, 0),
            Instruction::new(Opcode::Subtract, 0, 1, 2),
            Instruction::new(Opcode::ResultRow, 2, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Null]]);
    }

    /// MC/DC vector (obligation `vm_766`, `try_to_integer`'s decision
    /// `r.fract() == 0.0 && r.is_finite() && in_i64_range(*r)`): baseline
    /// all three leaves true -- a whole, finite, in-range REAL converts.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_766__v1_whole_finite_in_range_converts() {
        assert_eq!(try_to_integer(&Value::Real(5.0)), Some(5));
    }

    /// MC/DC vector (obligation `vm_766`): leaf A (`fract() == 0.0`)
    /// false -- a fractional REAL never converts. Independence pair for
    /// A against `mcdc__vm_766__v1_whole_finite_in_range_converts`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_766__v2_fractional_real_does_not_convert() {
        assert_eq!(try_to_integer(&Value::Real(5.5)), None);
    }

    /// MC/DC vector (obligation `vm_766`): leaf B (`is_finite()`) false
    /// -- an infinite REAL never converts (its `fract()` is NaN, so leaf
    /// A is collaterally false too, per IEEE-754: no infinite value is
    /// whole-valued). Exercises B's false branch alongside A's.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_766__v3_infinite_real_does_not_convert() {
        assert_eq!(try_to_integer(&Value::Real(f64::INFINITY)), None);
    }

    /// MC/DC vector (obligation `vm_766`): leaf C (`in_i64_range`) false,
    /// leaves A and B true -- a whole, finite REAL outside `i64`'s range
    /// never converts. Independence pair for C against
    /// `mcdc__vm_766__v1_whole_finite_in_range_converts`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_766__v4_out_of_range_whole_real_does_not_convert() {
        assert_eq!(try_to_integer(&Value::Real(1e30)), None);
    }
}
