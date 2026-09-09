# ADR 0017: `engine` — the client-facing seam over execution modes

## Status

Accepted (#295)

## Decision

`db_core::engine` is the one interface a client drives after deciding a
file's mode. Row, batch and stream each implement `trait Engine`; the
client never branches on mode again.

```rust
pub trait Engine {
    fn open(path: &Path) -> Result<Self, EngineError> where Self: Sized;
    fn mode(&self) -> Mode;                                        // Row | Batch | Stream
    fn run_query(&mut self, sql: &str) -> Result<QueryResult, EngineError>;
    fn explain_plan(&self, sql: &str) -> Result<Vec<PlanRow>, EngineError>;
    fn explain_opcodes(&self, sql: &str) -> Result<Vec<OpcodeSection>, EngineError>;
    fn stats(&self) -> FileStats;
}
```

## Structure

- `src/engine.rs` — trait and client types. `#[cfg(feature = "engine-row")] pub mod row`.
- `src/engine/row.rs` — `RowEngine`: `storage::row` (pager, header, b-tree)
  driven through `vm::row`. Session state: one `Rc<RefCell<Pager>>`, a
  catalog cache (`sqlite_master`, invalidated by `CREATE`/`DROP`/`ALTER`),
  the autocommit flag carried across `run_query` calls.
- `src/engine/row/adapter.rs` — the ADR 0008 boundary implementors:
  `StorageFactory: CursorFactory`, `PagerTransaction: Transaction`,
  `BtreeSchemaStorage: SchemaStorage`, `TableCursorAdapter`/`IndexCursorAdapter: Cursor`.
  Moved from sqlite-rs `src/vdbe/adapter.rs`.
- `src/engine/row/stats.rs` — `load_stats`: `sqlite_stat1` → planner
  `Stats`. Moved from sqlite-rs `src/planner.rs`.
- Feature `engine-row = ["storage-row", "parser-row", "vm-row", "codegen-row"]`, in `default`.
- `tests/unit/engine_public_api_test.rs` — black-box, through the trait and
  through `Box<dyn Engine>`, on a temp copy of a committed fixture.

## Client types

Owned, mode-independent, convertible from every mode's own types. Not a
third engine value model: `value::Value` (row) and `vm::batch::Value`
(batch) stay distinct (ADR 0010, ADR 0014).

| Type | Shape | From |
|---|---|---|
| `Cell` | `Null \| Int(i64) \| Real(f64) \| Bool(bool) \| Text(String) \| Blob(Vec<u8>)` | `value::Value` (lossless), `vm::batch::Value` (lossless; `Bool` is batch-only) |
| `QueryResult` | `{ columns: Vec<String>, rows: Vec<Vec<Cell>> }` | — |
| `PlanRow` | `{ id: i64, parent: i64, detail: String }` | row `EqpRow` (drops `notused`), batch `PlanNode` (identical fields) |
| `OpcodeSection` | `{ label: String, rows: Vec<OpcodeRow { addr, opcode, operands }> }` | row: one `main` section from `vm::row::explain`; batch: `codegen::batch::OpcodeSection` per phase |
| `FileStats` | `Row { page_size, page_count, freelist_pages } \| Batch { row_groups, rows } \| Stream { bytes_parsed, lines }` | header / footer / parse progress |
| `EngineError` | `{ kind: Open \| Parse \| Compile \| Execute \| Unsupported, message: String }` | every engine error, via `Display` |

## Object safety

Clients hold `Box<dyn Engine>` and switch the live engine per file, so:
no associated types (one concrete `EngineError`), and `open` carries
`where Self: Sized`. db-core never names `dyn Engine` itself; the
`dyn` at the ADR 0008 boundary (`Box<dyn CursorFactory>` etc.) is inside
`engine/row.rs` and `adapter.rs`, which join `MVL_LIMIT_EXCLUDE` for the
same reason `vm/row/vm.rs` is there.

## Semantics

- `run_query` splits on top-level `;`, runs each statement in order,
  returns the last non-empty result set. `SELECT` compiles with
  `sqlite_stat1` stats and runs on a read-only `Vm`; everything else runs
  on a writable `Vm` and, outside `BEGIN … COMMIT`, flushes the pager on a
  clean `Halt`. Column labels: schema-derived for a single-table `SELECT`,
  `column1..N` otherwise.
- `explain_plan` accepts one `SELECT`; `explain_opcodes` accepts one
  statement of any kind. Neither executes.
- `stats()` is header-only for row; no scan.

## Consequences

- db-core opens a `.sqlite` file and runs SQL end-to-end without sqlite-rs.
- sqlite-rs re-points its `vdbe::adapter`/`planner::load_stats` at
  `db_core::engine::row::{adapter, stats}` and deletes its copies
  (follow-up ticket).
- Batch (`engine::batch` over `storage::column` + `vm::batch`) and stream
  (blocked on a stream VM) are follow-up tickets; their types already fit
  the table above.
- Not in the seam: `.dot` commands, `PRAGMA` query shortcuts, output
  rendering, mode sniffing from file contents. Those are client concerns.
