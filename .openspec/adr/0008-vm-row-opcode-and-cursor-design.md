# ADR 0008: `vm::row`'s opcode set and cursor abstraction

## Status

Accepted.

## Decision

**Instruction shape.** `vm::row` executes the SQLite VDBE instruction
model literally:

```rust
pub struct Instruction {
    pub opcode: Opcode,     // bare tag, one variant per VDBE opcode
    pub p1: i32,
    pub p2: i32,
    pub p3: i32,
    pub p4: P4,             // dynamically typed fourth operand
    pub p5: u16,
    pub comment: Option<String>,
}
```

`Opcode` lists every VDBE opcode by name whether or not the dispatch
loop implements it yet; the compare-and-jump opcodes (`Eq`/`Ne`/`Lt`/
`Le`/`Gt`/`Ge`) are fused jumps, not register-writing compares. The row
planner (`codegen::row`) emits `p1..p5` directly, so no operand
translation layer exists between planner and VM. This differs from the
batch VM's typed operands (ADR 0007) on purpose: row opcodes have no
variable-length operand lists.

**Cursor abstraction.** `vm::row` drives storage through its own
storage-agnostic traits and never depends on `db-storage` (ADR 0006):

- `vm::row::cursor::Cursor` -- positioned access to one table or index
  b-tree (`rewind`/`next`/`prev`/`last`/`seek*`/`column`/`rowid`/
  `payload`/`insert`/`delete`, ...). `column` and `rowid` return
  `Option`; the dispatch loop, which knows what a missing row means for
  a given opcode, turns `None` into `ExecError::NoCurrentRow { opcode,
  slot }`. A cursor never panics on an unpositioned read.
- `vm::row::cursor_factory::CursorFactory` -- opens cursors for
  `OpenRead`/`OpenWrite` by root page.
- `vm::row::transaction::Transaction` -- the hook a consumer's pager
  installs to observe `BEGIN`/`COMMIT`/`ROLLBACK`.
- `vm::row::schema_storage::SchemaStorage` -- the hook for writing
  `sqlite_master`/`sqlite_stat1`/`sqlite_sequence` rows.

An in-memory `EphemeralTableCursor` implements `Cursor` for tests and
for the VM's own ephemeral tables. `vm::row::cursor_conformance`
publishes the trait-level conformance checks (including the `None`
case) so any external implementation can prove it satisfies the same
contract the in-memory one does.

**The storage adapter lives in the embedding application**, at the
composition root -- not in `db-core` and not as an optional feature of
`db-storage`.

## Consequences

- The `dyn Cursor` boundary is the one place `vm::row` uses dynamic
  dispatch; `src/vm/row/{vm,cursor,cursor_factory,cursor_conformance}.rs`
  are the documented exclusions from the qualified-subset gate (ADR
  0015).
- `vm::row::value` is `db_core::value` (ADR 0010); the VM has no value
  type of its own.
