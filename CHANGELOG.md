# Changelog

All notable changes to db-core. Format follows [Keep a Changelog](https://keepachangelog.com/), versioning follows [SemVer](https://semver.org/). Pre-1.0: minor bumps may break the public API.

**Versioning policy:** one crate, one version, one tag per release.

## [0.88.1] - 2026-09-11

### Fixed

- **`emit::batch`'s generated `Select` literal was missing the `scope` field** (introduced when ADR 0018 added `Select::scope`): any consumer using `emit-batch` to generate a standalone Rust program from a query (column-rs's codegen-vs-interpreter parity tests) failed with `rustc` error E0063 on the generated source. `scope: None` is now always emitted (batch queries never carry a stream-only `SINCE`/`UNTIL` scope).
- **`vm-stream` failed to compile alone** (db-core#356): `threshold_holds` (#309) needs `parser::ast::BinaryOp` at runtime, but the `vm-stream` Cargo.toml feature didn't declare `parser-row` as a dependency -- invisible under `default-features`, only hit by a minimal-features consumer (loglume: `storage-stream`+`vm-stream` only). Now `vm-stream = ["vm-batch", "parser-row"]`.
- **`parser-column`'s expression validator failed to compile alone** (same root cause as db-core#356, different call site): the range-vector (`count_over_time(...) RANGE ...`) validation arm unconditionally referenced `vm::stream::RangeAggFunc`, breaking a `vm-batch`-only build (column-rs's real feature set). Now `cfg`-gated: without `vm-stream`, any range-vector call is rejected outright (correct -- it could never execute anyway) instead of failing to compile.

## [0.88.0] - 2026-09-11

### Added

- **`LIKE`/`NOT LIKE`/`GLOB` in batch/stream `WHERE` clauses** (#352): `parser::column::validate_expr` gained an `ExprKind::Like` arm, `vm::batch` gained `MapOp::Like`/`MapOp::Glob` (delegating to the existing `functions::like_match`/`glob_match`), and `codegen::batch::compile_expr` lowers it; `codegen::stream` needs no change since it reuses `codegen::batch`'s compiled expressions verbatim. `LIKE ... ESCAPE` is explicitly rejected rather than silently ignored.

### Fixed

- **`collect_expr_columns` dropped columns referenced only in a `LIKE` predicate** (#352): surfaced as wrong results for `WHERE col LIKE ... GROUP BY col` on batch/stream, since the column wasn't pre-loaded before `Filter`.

## [0.87.0] - 2026-09-11

### Added

- **Standing queries** (#309, ADR 0018 phase 6): a compiled program plus an interval and a `for` duration, evaluated on demand in the client process. `engine::stream::StandingQuery` owns the SQL text and compiles it once, caching the resulting `Program`; `poll()` re-runs it through the same execution path as one-shot `run_query`, so a standing query never diverges from what a fresh query would return. `vm::stream::EmitMode` (`Rows`/`OnChange`/`Threshold { op, threshold }`) decides whether a re-evaluated result set constitutes a fire: `OnChange` fires on transitions (nothing→something, or result-set change); `Threshold` fires once when a range-vector's single reduced value crosses a comparison and holds for at least `for_duration` (Loki ruler semantics — never per-poll while the condition persists). Client-driven, not a background thread: the caller decides when to call `poll()` and reads `StandingQuery::interval()` as a hint for cadence. Feature `engine-stream`.

## [0.86.1] - 2026-09-11

### Fixed

- **`StreamEngine` format detection** (#348): `StreamEngine::open_with_budget` now calls `storage::stream::detect::detect` to pick the right parser for the opened file instead of hardcoding `SyslogParser`. A logfmt or JSON-Lines file opened through `StreamEngine` now parses correctly with the detected parser; rotation/truncation triggers re-detection.

## [0.86.0] - 2026-09-11

### Added

- **Cross-mode queries epic** (#317): the row→batch adapter (ADR-0019, #314) presents a SQLite table as a `vm::batch::Batch`/`Source`, the lookup side of a hash join. `engine::cross_mode::scan_table_as_batch` reads through `RowEngine`'s own b-tree (the row VM never runs) and materializes requested columns with a new `value::Value → vm::batch::Value` conversion. `RowTableSegment`/`RowTableSource` wrap the snapshot for the batch VM's existing `HashBuild`/`HashProbe` (no new opcodes, same join planner). Whole-table materialization v1: rowid-alias substitution, NULL-padding, BLOB columns rejected (no batch equivalent).
- **EXPLAIN labeling for cross-mode joins** (#315): `TableStats` gains an optional `source` label (table's mode and file, e.g. `"sqlite hosts.sqlite"`) shown in `SCAN` node detail; untagged tables (single-mode engines) report plain SCAN unchanged. Sets up future EXPLAIN to name which side is stream/batch/sqlite in a cross-mode join.
- **ADR-0019 documented** (#312): design for cross-mode queries, recorded as the basis for #314/#315.

### Changed

- **`#316` closed**: no parser work needed — existing `FromClause`/`Join` AST already parses multi-source `FROM x JOIN y ON ...`; the remaining gap (routing `TableRef` to the right engine) belongs to future runtime resolver work, not this epic's v1.

## [0.85.0] - 2026-09-11

### Added

- **`engine::stream::StreamEngine`** (#305, ADR 0018 phase 2): full SQL over a live syslog file through the batch VM with zero new opcodes. Tail-first open into a `Ring`; parse → `expand_star` → severity-literal rewrite → `codegen::batch::compile` → `vm::engine::run` over one `StreamSegment` per ring segment. `refresh()`, `segments(cols)`, `tail_source(cols, ..)`, `stats() → FileStats::Stream`, `tables()`. Feature `engine-stream`.
- **`storage::stream::adapter`**: `StreamSegment: vm::batch::Segment` materializes only `Program::columns_to_load()` (unknown column is a `SegmentLoad` error, never NULLs); `TailSource: vm::batch::Source` yields appended complete lines batch-at-a-time.
- **Severity-literal rewrite** (`engine/stream/rewrite.rs`): `severity >= 'WARN'` becomes `>= 13` before planning; unknown names are `Compile` errors. Moves to `codegen::stream` with #306.
- Benchmark `stream_materialize`: typed/dictionary column filter ~430 M rows/s; all-columns load ~7 M rows/s (string allocation), recorded in ADR 0018 §Consequences.

### Fixed

- Syslog timestamp parse accepted only RFC 3164 space-padded days; `Sep 9` shifted hostname, tag and message on every such line (#332).
- `src/lib.rs::VERSION` had drifted from `Cargo.toml` (0.84.0 vs 0.84.1) and the 0.84.1 changelog entry displaced the file header; both repaired.

## [0.84.1] - 2026-09-11

### Fixed
- `CREATE TABLE`/`CREATE INDEX` over an existing name now fails with proper error messages, matching SQLite's behavior (#299)

### Changed
- MC/DC test coverage expanded for storage-layer multi-leaf boolean decisions (131/132 obligations discharged)

## [0.84.0] - 2026-09-10

### Added

- **ADR 0000 -- the charter** (#324): what db-core is for (a safe, auditable SQLite in Rust) and the invariants that may never change while adding other modes -- the *SQLite profile* (`parser-row, vm-row, codegen-row, storage-row, engine-row`) has a first-party-only closure, `unsafe` only at the named carve-outs (`storage/row/vfs/fcntl.rs`: 2), and compiles no batch/column/stream file. Numbered ADRs must be consistent with it.
- **`make check-sqlite-profile`** (PR gate, `tools/check_sqlite_profile.py`): measures those three invariants from rustc's dep-info for the profile build -- 122 files, `{db-core}`, 2 `unsafe` sites -- not from a hand-maintained module list.
- **`tests/unit/layer_isolation_test.rs`**: the SQLite side never names `vm::batch`/`vm::engine`/`vm::stream`/`codegen::batch`/`storage::column`/`storage::stream`/`engine::{column,stream}`, and they never name the SQLite side (ported from sqlite-rs). Comment lines are ignored; code is not.
- **`.github/workflows/assurance.yml`** (weekly + on demand): `make check-coverage-profile` -- the coverage floor measured on the SQLite profile, not `--all-features` -- and the MC/DC discharge dashboard, as job summaries. First scheduled job in this repo.

### Changed

- **`codegen-row` no longer implies `vm-batch`.** The #153 note that `codegen::row` shared `vm::batch::AggFunc` was stale -- the row planner carries `P4::AggFunc { name, arity, collation }`, its own variant -- and the implication compiled `src/vm/batch.rs` into every sqlite-rs binary. `check-features` and the new profile gate both pass; consumers that relied on `codegen-row` pulling in `vm-batch` must name it.

## [0.83.1] - 2026-09-10

### Fixed

- **`src/storage` at db-core's full lint tier** (#289; precondition for the charter, #324). All 98 `as` casts under `storage/row` and `storage/column` reviewed: untrusted file integers now go through `try_from` with typed errors -- the Parquet reader accepted a negative `num_values`/`page_size`/chunk offset from a corrupt footer as a wrapped-around `usize` (`FileError::InvalidMetadata`, `ReadError::InvalidValueCount`, `ThriftError`/`EncodingError`/`SnappyError::InvalidLength` are new); rowid varints and the DELTA_BINARY_PACKED 64-bit delta are reinterpreted in one documented place each (`rowid_from_varint`/`rowid_to_varint`, `u64_bits_as_i64`); `local_payload_size` returns `usize`. The `storage`-wide `#![allow(cast_*)]` is gone (kept only on `storage::stream`, whose storage layer #304/#305 are still building).
- `check-mvl-limit` exemptions are named boundary files, not `src/storage/*`: the VFS `dyn` boundary, the two `unsafe` carve-outs, and the Parquet zero-copy reader (designated, ADR 0016 §Gates). `check-panic-allows`: `EXEMPT` is empty again -- `cfg(any(test, ...))` regions count as test code, and `lock_probe` lives in `tests/helpers/`.

## [0.83.0] - 2026-09-10

### Added

- **`storage::stream` foundation** (#304, ADR 0018 §Storage; PR #329) -- the live-file layer under the stream engine. `LogFile`: two line-aligned cursors (`tail_off`, `head_off`), 256 KiB block reads in both directions; `open` positions at the last newline reading at most one block; `read_tail(min_bytes)` walks backwards with partial-line carry so the last N lines cost O(bytes shown); `refresh()` reads forward and reports `NoNew | New(blocks) | Truncated` (`head_off > len` repositions). Only complete lines leave the module. `Segment`: the owned, `'static` form of a parsed block -- bytes behind `Arc<[u8]>`, `raw`/`message`/high-cardinality strings as `(u32, u32)` spans into them, dictionaries owned, Tier-2 columns owned, `minmax` over event and observed timestamps built at seal, `overlaps_event`, `dict_contains`, split at `SEGMENT_MAX_ROWS` (4096); a string not inside the block becomes `None`, never a panic. `Ring`: hot segments under a byte budget, whole-segment eviction from the tail, `push_tail` for backwards fill (returns the segment that did not fit), the head segment always kept, `overlapping_event(range)`, `set_budget`.
- `LogBatch::observed_ts_ns` (when a line was read) alongside `timestamp_ns` (what it says), plus `fill_observed_ts`.
- Fixture `tests/fixtures/stream/syslog-1k.log`.

### Fixed

- `SyslogParser::parse_batch` silently capped every batch at `BATCH_SIZE` (256) rows regardless of `max_lines`; the caller now bounds the batch and `BATCH_SIZE` is only the initial capacity.

## [0.82.0] - 2026-09-10

### Added

- **`engine::column::BatchEngine`** (`engine-column`, on by default; implies `storage-column`, `parser-column`, `vm-batch`, `codegen-batch`) -- the batch `Engine` (#325, #326, #327): one Parquet file as one table named after the file stem, ported from column-rs's `QueryEngine` single-file case. `open` (mmap + footer), `run_query` (parse -> `expand_star` -> `codegen::batch::compile` -> `vm::engine::run` over one `Segment` per row group, decoding only `columns_to_load()`), `explain_plan`/`explain_opcodes` (the batch planner's `PlanNode`/`OpcodeSection`, mapped field-for-field; footer row-group/row counts feed the plan), `stats` (`FileStats::Batch`), `tables` (one `TableInfo`; Parquet physical types as `type_name`). `JOIN`, `IN (SELECT ...)` and window functions report `Unsupported` -- they need column-rs's multi-table session. A syntax error is `ErrorKind::Parse` (column-rs's `QueryEngine` reports it as an unknown column).
- `engine::single_statement` is shared by the row and column engines.
- `make check-features` covers `engine-column`; `src/engine/column.rs` joins `MVL_LIMIT_EXCLUDE` (`ParquetFile<'a>`/`RowGroupSegment<'a, 'm>`, the storage boundary's lifetimes -- same exemption as `engine/row.rs`).
- Fixture `tests/corpus/fixtures/parquet/production.parquet` (from column-rs).

## [0.81.1] - 2026-09-10

### Fixed

- **`IN (...)` on a non-unique index returned only the first row per value** (#298). `try_compile_in_list_seek` did one `SeekIndexEq` per value and emitted a single row; with three rows sharing the key, `WHERE a IN (1)` returned one where sqlite3 returns three. Each value is now a bounded index walk (`SeekIndexGE` → `IdxCompareGT` stop → `IdxNext`), the same loop `BETWEEN` always used.
- **Equality on a secondary index now seeks** (#298). `WHERE col = lit` / `lit = col` (constant operand, matching affinity) was the one comparison shape the single-table seek family did not recognize, so it fell through to a full scan even though `>`/`BETWEEN`/`LIKE 'p%'`/`IN` on the same index all seeked. It is now the degenerate `BETWEEN lit AND lit` walk -- for `SELECT`, and through `try_compile_range_row_seek` for `UPDATE`/`DELETE` too. `EXPLAIN QUERY PLAN` reports sqlite3's wording, `SEARCH t USING INDEX i (col=?)`; an aggregate with such a `WHERE` reports the same (was `SCAN`).
- **No seek on a non-`BINARY` index** (#298). The index b-tree is stored and searched in BINARY order (Tier 0), so every `SeekIndexGE`-based shape on a `NOCASE`/`RTRIM` index -- `BETWEEN`, `>`, `LIKE 'p%'`, and now `=`/`IN` -- descended to the wrong leaf: `WHERE name = 'alice'` on a `NOCASE` index returned 0 of 3 rows, `name > 'alice'` 0 of 2. `find_leading_index` (the one choke point for all of them and their `EXPLAIN QUERY PLAN` mirrors) now requires a `BINARY` leading collation; such indexes fall back to the scan, whose filter applies the collation. Regression fixture `tests/corpus/fixtures/btrees/collate_nocase.db` (built by sqlite3, since db-core's DDL does not accept `COLLATE` in `CREATE INDEX`).
- Consumers asserting program shape: `IN (...)` no longer emits `SeekIndexEq`; it emits the `SeekIndexGE`/`IdxCompareGT`/`IdxNext` walk per value (sqlite-rs's `in_list_matches_exactly_the_listed_values` pins the old opcode).
- Shared `emit_bounded_index_walk` replaces three copies of the walk; `as_bounds` is the one shape-recognizer for `BETWEEN`/`=` used by all three eligibility mirrors, so `EXPLAIN QUERY PLAN` cannot drift from what compiles.

## [0.81.0] - 2026-09-09

### Added

- **`engine` -- the client-facing seam over the execution modes** (#295, ADR 0017). `trait Engine { open, mode, run_query, explain_plan, explain_opcodes, stats }`, object-safe (`Box<dyn Engine>` is how db-studio switches the live engine per file), with one concrete `EngineError { kind: Open | Parse | Compile | Execute | Unsupported, message }`. Client types, owned and mode-independent: `Cell` (lossless `From` both `value::Value` and `vm::batch::Value`; shell-style `Display`), `QueryResult { columns, rows }`, `PlanRow { id, parent, detail }` (row's `EqpRow` and batch's `PlanNode` are already this shape), `OpcodeSection { label, rows: Vec<OpcodeRow> }`, `FileStats::{Row, Batch, Stream}`.
- **`engine::row::RowEngine`** (`engine-row`, on by default; implies `storage-row`, `parser-row`, `vm-row`, `codegen-row`): a SQLite-format file through `storage::row` + `vm::row`. The ADR 0008 boundary implementors (`StorageFactory`, `PagerTransaction`, `BtreeSchemaStorage`, cursor adapters) and the `sqlite_stat1` reader moved in from sqlite-rs (`src/vdbe/adapter.rs`, `src/planner.rs`) as `engine::row::{adapter, stats}` -- db-core can now open a `.sqlite` file and run SQL end-to-end without sqlite-rs. Multi-statement `run_query`, catalog cache invalidated by DDL, autocommit carried across calls.
- `make check-features` covers `engine-row`; `src/engine/row.rs` and `adapter.rs` join `MVL_LIMIT_EXCLUDE` as ADR 0008 boundary implementors.

### Fixed

- `Cargo.lock` regenerated for 0.80.0 (a4789b8 bumped `Cargo.toml` without it; `--locked` builds failed on `main` and on the `v0.80.0` tag).

## [0.80.0] - 2026-09-09

### Added

- `storage::stream`: `Facility` enum and full syslog field extraction (#296). (Entry added retroactively in 0.81.0 -- the release commit carried no changelog.)

## [0.79.0] - 2026-09-09

### Added

- **`storage::stream`** (`storage-stream`, ADR 0006's third mode): stream-oriented storage for push-driven log sources — `SyslogParser` (RFC 3164/5424 line parser) filling a `LogBatch` (columnar per-field store: timestamp, severity, source, resource, message, plus `FieldStore` for structured fields) that `vm::stream::StreamFilter` consumes. Std-only. `storage-stream` implies `vm-stream`; on by default.

### Fixed

- MC/DC snapshot drift on `main`: `src/value.rs` and `codegen/row/expr/value.rs` collided on `value_106`/`value_108` after 0.78.0's line shifts, and `scalar_91` had moved to `scalar_92`. `codegen/row/expr/value.rs` is now `expr_value.rs` (module name unchanged via `#[path]`); tags remapped; snapshot regenerated.
- `make check-mcdc-fresh` (in `make ci`): regenerates the obligations snapshot to a scratch file and compares it byte-for-byte with `tests/mcdc/obligations.json`. `unit_mcdc_discharge` only checks that tagged tests name ids that *exist*, so a stale snapshot passed until the next regeneration surfaced every collision at once — which is how the drift above shipped in 0.78.0 and 0.78.1.
- `make check-features` now includes `storage-stream`.

## [0.78.1] - 2026-09-09

### Fixed

- **`storage-row` implies `parser-row`.** 0.78.0 did not compile with `--no-default-features --features storage-row`: `storage::row::schema::ddl_reader` calls `TableSchema::with_computed_rowid_alias`, which is `parser-row`-gated. db-storage had carried this as `db-core = { features = ["parser-row"] }`; the #290 merge dropped the implication. Caught by trigrep (the only consumer that enables `storage-row` alone).
- `benches/vm_opcodes.rs` imported `vm::row::Value`, removed in 0.78.0 (that change updated the changelog, not the bench) — `cargo clippy --all-targets`, i.e. `make lint`/`make ci`, failed on tagged `main`. Now `db_core::value::Value`.
- `make check-features` (in `make ci`): `cargo check --no-default-features` for each of `storage-row`, `storage-column`, `parser-row`, `vm-row`, `codegen-row`, `vm-batch`, `codegen-batch`, `emit-batch` — every feature must build standalone.

## [0.78.0] - 2026-09-09

### Changed

- **db-storage is now `db_core::storage`** (#288 follow-up, ADR 0016 rewritten). The `db-storage` workspace member from 0.77.0 is gone: its source lives at `src/storage/` (`storage::row`, `storage::column`), one crate, one `Cargo.toml`, one lint bar. Features: `storage-row`, `storage-column` (pulls the only two third-party deps, `memmap2` + `ruzstd`, both optional), `storage-test-support`; `storage-row`/`storage-column` are on by default. The `lock_probe` test-helper `[[bin]]` moved with it. Consumers replace `db-storage = { git = ..., package = "db-storage" }` with a `storage-*` feature on their existing `db-core` dependency and `db_storage::` with `db_core::storage::`.
- `storage` is scoped off db-core#225's `cast_*` tier and the `check-mvl-limit` gate, and two of its test-support sites are `EXEMPT` in `check-panic-allows` — all tracked as a worklist in db-core#289. `deny.toml` now allows `MIT` for the `storage-column` closure (`ruzstd`, `twox-hash`).
- MC/DC: obligations regenerated; `storage::row::btree::{table,index}/{insert,delete}.rs` renamed to `table_insert.rs`/`table_delete.rs`/`index_insert.rs`/`index_delete.rs` (module names unchanged via `#[path]`) so their basename-keyed obligation ids can't collide with `codegen::row::stmt::{insert,delete}`.
- `vm::row` no longer re-exports `value::{Value, Collation, TextEncoding, compare_text, format_real}` (the `vm/row/value.rs` shim is gone). Callers use `crate::value::*` directly, so `vm::row::Value` can't be conflated with the distinct, columnar `vm::batch::Value`.

## [0.77.0] - 2026-09-09

### Changed

- **Absorbed db-storage as a workspace member** (#287, ADR 0016). Merged via `git subtree` (history preserved) into `db-storage/`, now a workspace member with a `path` dependency on db-core instead of a git/tag pin. `make ci` runs both crates' gates; `make test-storage` runs db-storage's suite alone. The standalone `t-rust-db/db-storage` repo is superseded — downstream consumers (sqlite-rs, column-rs, trigrep) need to repoint at this repo's `db-storage/` subdirectory.

## [0.76.9] - 2026-09-09

### Fixed

- **Range-seek bounds accept constants beyond bare literals** (#280). `is_supported_operand` only recognized a plain literal or bind parameter as a range-seek bound, so an uncorrelated scalar subquery (`x > (SELECT avg(x) FROM t)`), constant arithmetic (`x > 1 + 2`), or a `CAST` of a literal forced the whole predicate back to a full scan even though the value is loop-constant and already computed into a register once, before the seek. `try_compile_between_seek`, `try_compile_forward_comparison_seek`, and `try_compile_range_row_seek` now accept those shapes via the new `is_constant_operand`; a NULL bound (only reachable now) seeks an empty range instead of matching everything. `EXPLAIN QUERY PLAN`'s own mirror of this eligibility check is widened identically so it keeps reporting `SEARCH` rather than `SCAN` for these queries.

## [0.76.8] - 2026-09-09

### Changed

- **Aggregate scans use an index range seek instead of `Rewind`** (#279). `compile_grouped_scan` and `try_compile_direct_agg_scan` (the implicit whole-table-group fast path) full-scanned the table even when `WHERE` was a single range/`BETWEEN` predicate on an indexed column. Both now try `try_compile_range_row_seek` (already used by `UPDATE`/`DELETE`) first -- `SeekIndexGE`/`IdxCompareGT`/`IdxNext` on the index, `IdxRowid` + `SeekRowid` to the table row -- and fall back to the existing `Rewind` scan for any other `WHERE` shape. The covering case (never opening the table cursor) is a follow-up. `EXPLAIN QUERY PLAN` reports the seek (`SEARCH t USING INDEX ix (col>?)`) for the aggregate and `GROUP BY` arms via the shared `range_row_seek_index_position` eligibility check (#282).

## [0.76.7] - 2026-09-09

### Fixed

- **`EXPLAIN QUERY PLAN` follows the compiled scan strategy** (#282). The plan was derived from the predicate and schema, not from the arm `compile_select_scan` dispatches to, so an aggregate, `GROUP BY` or `ORDER BY` query over an indexed range predicate reported `SEARCH ... USING INDEX` while the program `Rewind`s the table. `entry::scan_dispatch` now gates the direct-scan seek reports; `aggregate::find_index_only_count`/`find_index_only_sum` share the index-only fast paths' eligibility with EQP, which reports `SEARCH t USING COVERING INDEX ix (col=?)`, `SCAN t USING COVERING INDEX ix` and (index-ordered `GROUP BY`) `SCAN t USING INDEX ix` exactly when those compile. Scalar subqueries in top-level `WHERE` conjuncts get their own `SCALAR SUBQUERY n` / `CORRELATED SCALAR SUBQUERY n` node with the subquery's plan nested underneath. A program-vs-plan invariant test compiles every single-table scan shape and asserts `SEARCH` <-> seek opcodes, `SCAN` <-> `Rewind`.

## [0.76.6] - 2026-09-09

### Changed

- **Direct aggregate scan peels its first matching row out of the loop** (#281). `try_compile_direct_agg_scan` (implicit-group aggregates, no `GROUP BY`) carried an `Eq have_group, 0` + `Goto` on every scanned row to tell "first match: reset accumulators, snapshot the arbitrary row" from "later match: fold". The first match is now a separate pass, so the steady-state loop is `<WHERE> ; AggStep ; Next` -- two opcodes fewer per row; the `WHERE` predicate is compiled twice (its uncorrelated subqueries were already hoisted, so nothing runs twice). Result rows, zero-row behaviour (#287) and the bare-column snapshot are unchanged.

## [0.76.5] - 2026-09-09

### Changed

- **`vm::row` opcode audit, performance pass** (#257, #258, #259). `execute` sizes the register file from register operands only (`Opcode::register_operands`, `Instruction::max_register`), so an `Integer` literal or root page no longer reserves that many cells per run (`SELECT 1000000` 459 us -> 73 us). `AggStep` takes its accumulator instead of cloning it per row; pseudo cursors cache the parsed record header per slot (`GROUP BY` ~1.12x faster); `String8`/`Blob` allocate once; index-key opcodes borrow their `P4` collations; `compare_jump` clones only operands affinity can change; `concat` renders into one pre-sized buffer (a filtered `WHERE` scan ~1.3x faster). Result rows, error variants and `EXPLAIN` output unchanged.
- **Direct dispatch tests** for the 25 `vm::row` opcodes previously covered only through codegen roundtrips (#261).

### Fixed

- `Column` with a negative column index is `ExecError::MalformedInstruction` instead of a silent NULL; `Copy` honours `p3` extra registers (SQLite `OP_Copy` shape) and rejects a negative `p3`; `Return`'s `r[p1]` target is documented as sqlite-rs parity (#260).

## [0.76.4] - 2026-09-09

### Changed

- **Allocation-free `ORDER BY` compare and typed window partition key** (#266). `compare_for_order` compares `Str`/`Str` directly as `&str` instead of allocating two `String`s via `to_string()` (byte-identical ordering); `compute_window`'s partition key reuses the typed `GroupKey` (#263) instead of stringifying and joining every partition column per row, and `Rank`/`DenseRank`'s tie-break reuses `compare_for_order` directly. String `ORDER BY` is ~8x faster; string `PARTITION BY` is ~1.13x faster.

## [0.76.3] - 2026-09-09

### Changed

- **`Filter` produces a selection vector instead of eagerly compacting registers** (#265). `Opcode::Filter` records surviving row indices on the `Vm` instead of draining and rebuilding every live register; `Emit`/`GroupReduce`/`HashBuild` resolve it lazily (only the registers they actually read), while `Map`/`Reduce`/`Window`/`HashProbe`'s key-column input force an eager compaction first, reproducing prior behavior exactly. `Filter` alone with unused live registers is ~2.3x faster; a realistic `Map+Filter+Emit` shape is ~1.2x faster.

## [0.76.2] - 2026-09-09

### Changed

- **`LoadColumn` and `Segment::load` are zero-copy** (#264). `Batch.columns` and `Vm::registers` hold `Arc<Vec<Value>>` instead of `Vec<Value>`: `LoadColumn` is a refcount bump, not a per-cell clone, and `Segment::load`'s `Batch::clone` is a `HashMap`-of-`Arc` clone, not a deep copy. `Emit`'s move-not-clone optimization (#262) and `Vm::take_register` (#272) keep working via `Arc::try_unwrap`, falling back to a clone only when the register is still shared with its batch.

## [0.76.1] - 2026-09-09

### Added

- **`VmError::SegmentLoad { reason }`** -- the error a `Segment::load` implementation returns when the failure is outside the VM (a storage decode error, a missing row group). Exists so column-rs can stop NULL-filling a column whose Parquet decode failed (t-rust-db/column-rs#27); exhaustive `match`es on `VmError` gain an arm.

## [0.76.0] - 2026-09-09

### Changed

- **Joins run per segment, in parallel, without materializing the joined table** (#272). New `vm::engine::run_join_segments(left: Vec<S: Segment>, right, plan)`: the build side runs once into an `Arc`-shared `JoinTables` handle (`Vm::with_join_tables`/`Vm::join_tables`), then `probe ++ body` runs per left segment through the same morsel-driven `run` every single-table query uses, so the trailing `Combine` merges per-segment aggregates. Each segment's probe registers move straight into the body's `Batch` (`Vm::take_register`) -- the old `to_vec` + `InMemorySegment` clone round trip is gone. `run_join(&Batch, &Batch, plan)` stays as a single-segment wrapper. Motivation: the `t-rust-db/benchmark` parity `join` at 10M rows ran on one core at 108x DuckDB and 4.4 GB peak RSS.
- **`Opcode::HashProbe` allocates nothing per probe row for integer keys**: one reused key buffer, `JoinHashTable::for_each_match_slot` (no `Vec` per probe) + `value_at`, and payload cells cloned once into their destination column instead of a full payload-row clone per match.
- **BREAKING: `Segment::load` returns `Result<Batch>`** -- a segment may run a program (the join probe) or decode storage, and either failure is now a typed `VmError` instead of a panic. Implementors wrap their batch in `Ok`.

### Fixed

- **`COUNT` merged across segments came back as `Float`** (found by #272's per-segment join tests). `Combine` summed partial counts through the `SUM` path, so `SELECT COUNT(*)` over a multi-row-group file returned `3.0` where a single-segment scan returned `3`. Partial counts now merge as integers (typed error on `i64` overflow).

## [0.75.3] - 2026-09-09

### Changed

- **`GroupReduce` hashes a typed key instead of `to_string`/`join`** (#263). Adds `GroupKey(Vec<Value>)` alongside the existing `JoinKey`: same variant-tagged `Hash`, but derived equality (`Null == Null`) since `GROUP BY` groups NULLs together, unlike a join key. ~1.65x-1.71x at low/medium cardinality; a documented, accepted ~1.4x regression at unique-key cardinality (every row its own group), matching spike #183's finding.

## [0.75.2] - 2026-09-09

### Changed

- **`Emit` moves registers instead of cloning cells** (#262). Emit is terminal for the registers it lists, so each column is now removed (moved) out of the register map instead of borrowed and cloned into every output row; a register listed more than once in one `Emit` (e.g. `SELECT a, a`) clones only from the already-owned copy for the repeats. No change to `run_parallel`'s or `Vm::output`'s row-major shape.

## [0.75.1] - 2026-09-08

### Changed

- **Hot paths from the first profile** (#252, #253, #254; profiled with `make perf-profile`, #250). `codegen::row`: `TableBinding.schema` is `Rc<TableSchema>` and `Scope.catalog` is `Rc<[TableSchema]>`, threaded once from the planner entry through every scan strategy, so a `Scope` is one allocation instead of a deep copy (`compile_select_with_catalog` -24.5%). Parser: keyword lookup searches only the first-letter bucket, identifier scanning steps ASCII bytes directly, and `Parser::advance` no longer clones the consumed token (`tokenize/medium` -20%, `parse_select/medium` -14%). `vm::row`: `execute` hands rows over with the new `Vm::take_rows` instead of cloning them (a reused `Vm` starts each run empty), `Vm::reserve_registers` sizes the register file once from the program, and `EphemeralTableCursor` caches its current row index (`row::Column` scan loop -69%). Public signatures, program output and cursor contracts unchanged.
- **`make perf` is `std`-only** (#245) -- `criterion` is gone; `benches/common.rs` is the crate's own harness (warm-up, batched samples, min/median/p95, JSON under `target/perf/`). `Cargo.lock` is back to one crate. `[profile.bench] debug = "line-tables-only"`, `PERF_BUDGET_MS`, and `make perf-profile BENCH=<bench>` (Instruments' Time Profiler via `tools/perf_profile.py`) support profiling the same binaries (#250).
- **ADRs describe the architecture as it stands** (#227). All fourteen rewritten without provenance narration; six renamed; ADR 0015 records the testing strategy (tiers, gates, what gates a merge, what db-core leaves to db-storage, db-cli and the embedding engine). Makefile rationale comments point at it.

### Fixed

- `Cargo.lock` recorded db-core 0.74.1 after the v0.75.0 release commit; it is 0.75.0 again (and 0.75.1 with this release), so a plain `cargo` run no longer dirties the tree.

## [0.75.0] - 2026-09-08

### Changed

- **BREAKING: `codegen::batch::compile` and `explain` return `Result`** (#232, group 3) -- `compile(select) -> Result<Program, PlanError>`, `explain(select, stats) -> Result<Vec<PlanNode>, PlanError>` (was infallible). Before, a select item the planner could not classify compiled to a program that emitted nothing, `has_agg` silently flipped to `false`, and EXPLAIN dropped a join it could not plan (`UnsupportedJoinKind` swallowed). `codegen::batch::emit::{render_joined, render_semi_join, render_windowed}` return `Result<String, EmitError>` for the same reason; new `EmitError::Plan(PlanError)` and `PlanError::Internal(String)`.
- **Parser and batch-VM fallbacks are typed errors** (#232, group 4). `a.b.c.d` is `Invalid` (the wildcard arm silently dropped the fourth part); the batch validator rejects a `GROUP BY` expression with its span instead of dropping it from the bare-column comparison; `Tokenizer::tokenize` refuses SQL text longer than 4 GiB up front instead of saturating span offsets. New `VmError::MalformedProgram { opcode, reason }` for `HashBuild`/`HashProbe` with no key columns, `Emit` with no registers, a join payload narrower than its destinations, and a non-numeric partial aggregate (was: merged as `0.0`); `vm::engine::finalize` returns `Result` accordingly.
- **`emit` refuses what it used to render as `TABLE = ""`** -- a `SELECT` without `FROM`, a `FROM` subquery without an alias, or a `JOIN` against a subquery is `EmitError::Unsupported` instead of generated source that can never bind a file. The generated program's file matcher errors on a non-UTF-8 file name instead of binding it to a table called `data`.

## [0.74.1] - 2026-09-08

### Changed

- **Planner invariant fallbacks in `codegen::row` are `CodegenError::Internal`** (#232, group 2). New variant `CodegenError::Internal { reason }` ("planner invariant violated: ...") -- a codegen bug, distinct from `Unsupported`. 45 sites that defaulted a register/column/level lookup to 0 (or an empty schema entry) when the compiler's own invariant failed now return it; none is reachable from SQL, but a silently wrong program is no longer the failure mode. Exhaustive `match`es on `CodegenError` gain an arm.

- **Silent fallbacks in `vm::row` are typed errors or documented semantics** (#232, group 1 of 4). New `ExecError::RecordDecode { opcode, source }` for a corrupt record blob read through a pseudo-cursor (was NULL); `Sequence` on an unopened slot is `CursorNotOpen` (was a counter seeded from 0); `Variable` with a zero/negative index is `MalformedInstruction` (was NULL); `HashAggData` on a non-hash-agg cursor is `MalformedInstruction` (was empty accumulators). `SorterCursor::sorter_insert` / `HashAggCursor::hash_agg_find` refuse an undecodable record (`false` → VM error) instead of keying it as NULL; `InMemoryIndexCursor::insert` refuses a row narrower than its key. **`PseudoCursor::new(blob)` now returns `Result<Self, RecordError>`** (was an empty row on decode failure); use `PseudoCursor::default()` for an empty placeholder.

## [0.74.0] - 2026-09-08

### Added

- **Shared trigram-extraction helper** (#246) -- `functions::trigrams` (every overlapping 3-byte window of a string's raw UTF-8 bytes, allocation-free) and `functions::trigram_key` (packs a trigram into an i64 b-tree rowid key). Not wired into the scalar-function registry; a shared primitive for a future trigram-accelerated `LIKE`/`GLOB` index here, and for sqlite-rs's planned `sqlgrep` (t-rust-db/sqlite-rs#34), which needs the same byte-level trigram definition SQLite's own FTS tokenizer and tools like ripgrep/tgrep use.

## [0.73.0] - 2026-09-08

### Added

- **Phase-level performance report via `make perf`** (#224) -- three criterion benchmark suites (`benches/{parser,codegen,vm_opcodes}.rs`): tokenize + `parser::row::parse_select` over a short/medium/deep-nesting corpus, `codegen::row`/`codegen::batch`'s planner entry points over already-parsed ASTs, and ns/op for a representative opcode per execution shape in both `vm::batch::Opcode` and `vm::row::Opcode`. Report only, not wired into `make ci`; JSON estimates land under `target/criterion/`.

## [0.72.0] - 2026-09-08

### Added

- **Black-box `tests/unit` coverage for `codegen::batch`(+`emit`), the five root modules, and `vm::batch`/`vm::engine`/`vm::join`** (#223) -- four new test files (`codegen_batch_public_api_test.rs`, `codegen_batch_emit_test.rs`, `root_public_api_test.rs`, `vm_engine_join_public_api_test.rs`) plus extensions to `vm_batch_public_api_test.rs` and `parser_public_api_test.rs`, so every pub entry point named in the issue is called from at least one black-box test rather than only reached transitively through whatever an executor happens to call.

## [0.71.1] - 2026-09-08

### Changed

- **MC/DC vectors live with the code they test** (#235) -- the test-only `codegen::row::mcdc` module from #219 is gone; every tagged vector sits in a `mcdc_vectors` test module at the bottom of the file whose decision it discharges, with only the fixtures it uses. That module only existed to keep the moved files diff-able against sqlite-rs; db-core owns its own testing strategy (one of three core strategies, sqlite-rs holds a fourth). ADR 0013 amended to withdraw the drift-check clause; `tools/check_panic_allows.py` drops its `mcdc/` carve-out.

## [0.71.0] - 2026-09-08

### Changed

- **Panic lints are scoped to production code** (#230) -- `clippy.toml` `allow-*-in-tests` + `lib.rs` `cfg_attr(test, allow(...))` replace 46 per-module `#[allow]` headers; `tests/unit/*.rs` carry one canonical header. New `make check-panic-allows` gate (inside `make lint`): no `#[allow]` of a panic lint anywhere in production `src/`.

- **BREAKING: `vm::row::Cursor::column` and `Cursor::rowid` return `Option`** (db-core#231) -- `None` means "no current row" (nothing positioned the cursor, or its last `rewind`/`next`/`seek` returned false). Before, every in-tree implementor `expect`ed here, so a malformed program aborted the embedding process; the dispatch loop now maps `None` to the new `ExecError::NoCurrentRow { opcode, slot }`. This matches the trait's existing `idx_rowid`/`payload`/`current_blob` idiom. **Downstream implementors** (t-rust-db/sqlite-rs's storage-backed `TableCursor`/index cursors) must wrap their return in `Some(..)` and return `None` instead of panicking when unpositioned; `cursor_conformance::assert_cursor_conformance` now checks that (`assert_reads_with_no_current_row_are_none`).
- `codegen::batch::compile_window` returns `Result<Program>` (was `Program`) -- a window column missing from the load list is `PlanError::UnsupportedSelectItem`, not a panic. `explain_opcodes` already returned `Result`, so its callers are unaffected.
- New error variants, all replacing production `expect`s: `ExecError::NoCurrentRow`, `VmError::MissingWindowArgument { opcode, func }` (an `Opcode::Window` for `Lag`/`Lead`/`FirstValue`/`LastValue` with `arg: None`), `PlanError::NoJoinClause` (`compile_join` on a `SELECT` without a `JOIN`).
- Production `src/` now has **zero** `#[allow(clippy::expect_used)]`; `make check-panic-allows` (#230) enforces it with an empty exempt list.

## [0.70.2] - 2026-09-08

### Fixed

- **Deeply nested subqueries no longer overflow the parser's stack** (#226) -- `MAX_EXPR_DEPTH` only counted expression recursion, so `SELECT a FROM (SELECT a FROM (...))` aborted the process at ~100 levels on a 2 MiB thread and `SELECT (SELECT (SELECT ...))` / `EXISTS (...)` at ~50, with the expression guard intact. `parse_select_stmt` now charges `SELECT_DEPTH_COST` (6) units against the same budget; runaway nesting is a typed `Invalid("subquery nesting too deep")`. Measured caps on a 2 MiB stack: 31 derived tables, 21 scalar subqueries (real SQL nests 2-4 deep); expression nesting unchanged at 63 parens. `tests/unit/nesting_depth_test.rs` proves `codegen::row` and `codegen::batch` survive the deepest input the parser now accepts.

### Changed

- **Clippy panic gate extended** (#225): `string_slice`, `unreachable`, `todo`, `unimplemented`, `cast_possible_truncation`, `cast_possible_wrap`, `cast_sign_loss` are now `deny`. Seven `&str[a..b]` sites and 38 `as` casts replaced with total equivalents (`get`/`split_at_checked`/`try_from`/`from_ne_bytes`; new saturating `value::len_to_i64`); `JoinHashTable::hash_of` returns `usize`. No behaviour change.

## [0.70.1] - 2026-09-08

### Fixed

- **`INTEGER PRIMARY KEY DESC` is no longer treated as a rowid alias** -- `TableSchema::with_computed_rowid_alias` (moved in 0.70.0) accepted any inline `PRIMARY KEY`; SQLite gives the `DESC` form its own index and stores the column normally, so the column must not be substituted by the rowid. db-storage's retired hand-rolled detector had this rule; sqlite-rs's dump tests caught the drift (t-rust-db/sqlite-rs#19).
- **A string literal is accepted wherever an identifier is required** (`CREATE TABLE 't_data'(...)`, the form FTS5 writes for its shadow tables) -- `parser::row`'s `identifier()` now takes `TokenKind::String` as well as `TokenKind::Identifier`, matching SQLite's "string constant used as identifier" rule. Before, such a `CREATE TABLE` failed to parse and its rowid alias was silently `None`, so sqlite-rs's dump/export of the FTS5 fixture diverged from the oracle. MC/DC snapshot regenerated (the line shift re-tags 222 vectors).

## [0.70.0] - 2026-09-08

### Changed

- **Row schema catalog types have one home: `db_core::schema`** (ADR 0014, t-rust-db/sqlite-rs#19) -- `TableSchema`, `IndexSchema`, `IndexedColumn` and `ViewSchema` move out of `codegen/row.rs` into a feature-free leaf module; `codegen::row` re-exports them, so every existing path keeps resolving. `TableSchema::with_computed_rowid_alias` (needs the SQL parser) is gated on `parser-row`. db-storage re-exports the same four types instead of defining its own (its 0.6.0), the ADR 0010 pattern for `Value`, which is what lets sqlite-rs turn `src/codegen`/`src/planner` into facades without a per-statement schema copy.

## [0.69.0] - 2026-09-08

### Changed

- **`codegen::row` is now sqlite-rs's codegen, moved verbatim** (#219, ADR 0013) -- the 15.7k-line re-derived tree is deleted and replaced by t-rust-db/sqlite-rs's `src/codegen.rs` + `src/codegen/**` (as of `751e291`, v0.19.1; Lab271 synced `7701d18`) plus the pure half of its `src/planner.rs` as `codegen::row::planner` (`Stats`, `PlanCost`, `estimate_*`, `is_*_worthwhile`; `load_stats` stays storage-side). Only module paths changed (`crate::vdbe` -> `vm::row`, `crate::schema` -> `codegen::row`, `crate::planner` -> `codegen::row::planner`). Public API follows sqlite-rs: `dispatch::compile_statement(sql, schemas, views)` (the `views` argument is no longer optional; `compile_statement_with_views` is gone), `compile_select`/`compile_select_with_catalog[_and_stats]`/`compile_select_joined`/`compile_select_compound`, `explain_query_plan(select, schemas, stats_by_table, catalog) -> Vec<EqpRow>`, `output_column_names`, `leading_keywords`, `expand_with_clause`, `flatten_from_subqueries`, `push_down_where_predicates`, `resolve_views`, `ExpandViews`. db-core's two additions are marked `db-core#219` in place: the schema structs (`TableSchema` gains `with_computed_rowid_alias`, backed by the crate's own parser) and `dispatch.rs`'s `SELECT`/`WITH`/`EXPLAIN` arms (`compile_select_statement`, `explain_select_statement`, `compile_eqp_program`), ported from sqlite-rs's CLI `query.rs`. Brings the stats-driven access-path chooser, skip-scan, automatic indexes, N-way joins (`join_order`), `FULL`/`RIGHT` join levels, correlated-subquery hoisting/memoization, and the CTE/view flattening + predicate push-down passes (#117, #118, #144 land implemented). Supersedes #175, #212-#218, #216.
- **MC/DC snapshot regenerated** -- `tests/mcdc/obligations.json` had not been regenerated since #198, so `unit_mcdc_discharge` was checking stale ids; re-tagged the shifted `column`/`batch` vectors, padded `vm/batch.rs`'s module doc so its obligation ids stop colliding with `codegen/batch.rs`'s, and added vectors for every multi-leaf decision the moved tree brings (test-only `codegen::row::mcdc`).

### Added

- **Carry-overs onto the moved codegen** (#219) -- the three behaviours db-core's re-derived tree had grown that sqlite-rs's lacked, re-added as ordinary db-core-owned changes marked `db-core#219` in place: `HAVING` over a joined `GROUP BY`/aggregate (`aggregate/join.rs::flush_joined_group` filters the finalized group record through `substitute_aggregates` + a synthetic schema, exactly like the single-table `accum::flush_group`, and ahead of the `ORDER BY` re-sort); `DISTINCT` combined with `ORDER BY` on a join and on a `FULL JOIN` (dedup runs over the *sorted* output, before `LIMIT`/`OFFSET`, on a cursor past the sorter's pseudo cursor -- the three former `Unsupported` guards are gone); and a regression test pinning that a scalar/`IN` subquery may project a computed expression, which sqlite-rs's `single_result_expr` already allowed.

### Removed

- `codegen::row`'s re-derived entry points: `compile_select_join`, `compile_select_with_catalog(schemas, select)` (argument order now follows sqlite-rs: `(select, schema, catalog)`), `compile_eqp_program` in `eqp.rs` (now in `dispatch`), `expand_views(&mut Select, ..)` (now the `ExpandViews` trait over `Cow`), `CodegenError::{TooDeep, CircularView}`, `MAX_EXPR_DEPTH`.

## [0.68.1] - 2026-09-07

### Added

- **`codegen::row` schema richness** (#205) -- `TableSchema`/`IndexSchema` grow to a superset per new ADR 0012: `column_collations`, `without_rowid`, `strict`, `is_virtual`, `sql`, and per-index `unique` + `IndexedColumn { name, desc, collation }`. Carried through every in-crate test builder (db-core has no dependency on `db-storage`, so these are the only construction sites today); not yet consulted by codegen behavior itself -- that's #206/future work.
- **`codegen::row` dispatch parity** (#206) -- adds db-core's own `ViewSchema { name, sql }` plus `resolve_views`/`expand_views` (`subquery::views`, ported from sqlite-rs): a view reference in `FROM`/`JOIN` position expands to a `TableRefKind::Subquery` the same way `WITH`-clause CTEs already do, with a new `CodegenError::CircularView` guarding against a cycle. `compile_statement_with_views` threads `views` through dispatch; `compile_statement` is now a thin wrapper with an empty `views` slice, so every existing caller is unaffected. `UPDATE`/`DELETE` gain `compile_update_with_catalog`/`compile_delete_with_catalog` so a scalar/`IN`/`EXISTS` subquery in their `WHERE` clause can resolve another table, matching `SELECT`'s own `compile_select_with_catalog`. Adds `leading_keywords(sql) -> Vec<String>`, mirroring sqlite-rs's dispatch helper. `INSERT ... SELECT` remains out of scope (`compile_insert` still rejects it as `Unsupported`).

## [0.68.0] - 2026-09-07

### Added

- **`RIGHT`/`NATURAL`/`USING`/`CROSS` join codegen** (#208) -- `codegen::row`'s join gate now admits `JoinOp::Right` and `JoinOp::Cross` (the parser has produced this AST since #250; only codegen rejected them). `RIGHT JOIN` reuses the existing `FULL OUTER`'s second pass unchanged, since a right-outer null-extension is exactly that pass's right-hand half with no left-hand half. `NATURAL` synthesizes its join condition from the two tables' shared column names (case-insensitive), degrading to an unconditional join when none are shared, matching SQLite. `CROSS JOIN` with no `ON`/`USING` compiles as an unconditional nested loop instead of erroring; `CROSS JOIN ... USING (...)` still filters. `SELECT *` now also dedupes `NATURAL`/`USING`'s join-key columns instead of emitting them from both sides -- a latent bug for `USING` even before this PR. Aggregation over a join remains `INNER`/`LEFT`-only, unchanged; two-table joins only, N-way joins remain tracked in #118.

## [0.67.0] - 2026-09-07

### Added

- **`EXPLAIN QUERY PLAN` dispatch** (#175) -- `explain_query_plan` (`src/codegen/row/eqp.rs`) has been fully implemented since #94 but was never reachable from `compile_statement`: `EXPLAIN` wasn't in dispatch's keyword allowlist, so any `EXPLAIN`-prefixed statement fell straight to `Unrecognized`. Adds an `"EXPLAIN"` dispatch arm plus a new `eqp::compile_eqp_program` bridging `explain_query_plan`'s plain `Vec<EqpRow>` into a dispatchable `Program`. Scoped to `EXPLAIN QUERY PLAN` over a real catalog table with at most one `JOIN`, matching `explain_query_plan`'s own existing signature; bare `EXPLAIN` (opcode listing) parses but is rejected with a clear `Unsupported` (#55).

## [0.66.0] - 2026-09-07

### Added

- **`SELECT` with no `FROM` clause** (#175) -- a new `codegen::row::select::compile_select_no_from`, wired into `compile_select_with_catalog` whenever `query.from` is `None`: `SELECT 1`, `SELECT 2 IN (SELECT * FROM t6)`, etc. No table to scan, so the whole program is `<compile each column expr> -> ResultRow -> Halt`, run once (or zero times, if `WHERE` is present and evaluates false/`NULL`) -- no `Rewind`/`Next` loop at all. `*`/`table.*` (nothing to expand against) and `DISTINCT`/`GROUP BY`/`HAVING`/`ORDER BY`/`LIMIT` remain unsupported for this first cut, tracked on #175.

## [0.65.0] - 2026-09-07

### Added

- **`FILTER (WHERE ...)` for aggregates and window functions** (#67) -- `parser::row`'s grammar gains `FILTER (WHERE <expr>)` on any function call, carried on `ExprKind::FunctionCall` as a single `tail: Option<Box<FunctionTail>>` merging `FILTER`/`OVER` (rather than a second independent `Option<Box<_>>` field, which alone regressed the `MAX_EXPR_DEPTH` stack-overflow guard test). `codegen::batch` compiles the filter predicate and null-masks the aggregate's source register via a new `MapOp::MaskIf`, applied before `WHERE`'s `Opcode::Filter` so it shrinks in lockstep with everything else -- `Reduce`/`GroupReduce`/`Window`'s existing null-skipping does the actual filtering, no new VM control-flow opcode needed. Restricted to `SUM`/`AVG`/`COUNT` window functions per the SQL standard (ranking functions reject it with a clear `PlanError`); `codegen::row`'s row-at-a-time aggregates reject it outright rather than silently ignoring it.

### Fixed

- Closed #57 as resolved by the already-merged PR #66 -- the unification landed via a lighter parse-boundary approach than the issue's original plan, and #67 turned out to be the only real remaining gap.

## [0.64.0] - 2026-09-07

### Added

- **Compound `SELECT` (`UNION`/`UNION ALL`)** (#175) -- a new `codegen::row::subquery::compound::compile_compound_select` entry point, wired into `compile_select_with_catalog` whenever `query.compound` is non-empty. Each arm's plain single-table scan (no `JOIN`, no `GROUP BY`/aggregate) is materialized into one shared ephemeral table; a `UNION` chain also dedups against one shared ephemeral index keyed on every output column, generalizing the same `OpenEphemeral`/`IdxInsert`/`Found` membership dance `IN (SELECT ...)` already uses for a single-column key. N-ary chains work; every arm must project the same column count. A mixed `UNION`/`UNION ALL` chain, `ORDER BY`/`LIMIT` over the whole compound, `GROUP BY`/aggregation in an arm, and `INTERSECT`/`EXCEPT` (no such AST variant yet) remain unsupported, tracked on #175.

## [0.63.0] - 2026-09-07

### Added

- **Computed expressions in the `codegen::batch` `SELECT` list** (#198) -- `parser::column::validate_select` rejected any computed expression in the `SELECT` list (`SELECT -x`, `SELECT a || b`, `SELECT x * 2 FROM t` all failed at parse time), only accepting bare columns, `*`, aggregate calls, and window functions; `codegen::row` gained the row-level equivalent in #168, and `codegen::batch` already lowered `Binary`/`Unary`/`Concat`/`Neg` for `WHERE` (`MapOp::*`) but had no counterpart for `SELECT`. Validation now delegates to the existing `WHERE`-clause expression validator, and `codegen::batch` gains an `Item::Expr` projection kind compiled through the same `compile_expr`/`MapOp` machinery, with `EXPLAIN` and the AOT `emit::generate` path picking it up via the existing output-column-header renderers. A computed expression alongside an ungrouped aggregate, or alongside a window function, is still rejected -- neither compile path composes with those yet.

## [0.62.3] - 2026-09-07

### Fixed

- **Scalar/`IN` subquery may project a computed expression** (#175) -- `expr IN (SELECT ...)` and scalar `(SELECT ...)` subqueries required their single projected column to be a bare column reference, rejecting anything else (e.g. `x + 1`) as `Unsupported`. Any single non-star expression now compiles through the existing correlation-aware `compile_value` path; an aggregate call (`COUNT(*)`, `AVG(c)`, ...) is still rejected, now with a distinct message, since it needs whole-scan `AggStep`/`AggFinal` accumulation rather than a per-row `compile_value` call (left as follow-up work on #175).

## [0.62.2] - 2026-09-07

### Fixed

- **`codegen::row` programs open their own cursors** (#182) -- every program relied on the caller pre-wiring its cursor slots via `Vm::open_cursor` ahead of time, which breaks once a `CursorFactory` is installed (the sqlite-rs adapter): a scan that never emits `OpenRead` never gets a cursor, and `INSERT`/`UPDATE`/`DELETE`'s hardcoded `OpenWrite cursor, 0, 0` opened root page 0 instead of the real table. `select.rs`'s main/JOIN-right cursors and every DML/index-maintenance `OpenWrite` now carry the real `schema.root_page`. Blocked the sqlite-rs repoint; all 2,919 statements in its shadow-run corpus failed before this fix.

## [0.62.1] - 2026-09-07

### Fixed

- **`make test`/`make lint` restored** -- `tests/unit/vm_batch_public_api_test.rs` still imported `db_core::join::JoinKind` after #192/#193/#194 moved `join` under `vm` (#195). Also restores `make test-mcdc`, which the same module move had silently dropped to 0/70 discharged obligations; regenerated and re-tagged the snapshot, and added a regression test guarding against future cross-file obligation-id collisions.
- **`make check-deny`**: dropped the unmatched `MIT` entry from `deny.toml`'s license allow list -- db-core has zero dependencies, so nothing in the graph is MIT-licensed (#196).

### Changed

- `make test`/`make test-lib`/`make coverage` now exclude `tests/spike/` (throwaway experiments); added `make test-spike` to run them explicitly.

## [0.62.0] - 2026-09-07

### Changed

- **Module layout: Rust 2018 style, `emit` folded into `codegen::batch`, `join` moved under `vm`** (db-core#192, db-core#193). Breaking:
  - The nine `mod.rs` module roots under `codegen`/`parser`/`vm` are renamed to `<name>.rs` + `<name>/` siblings (pure rename, no behavior change); `[lints.clippy] mod_module_files = "deny"` now enforces this.
  - `db_core::emit::batch::generate` moves to `db_core::codegen::batch::emit::generate`. The crate-level `emit` module (a three-way `batch`/`row`/`stream` mirror of `vm`, with `row`/`stream` both unimplemented stubs) is retired: ADR 0007 already said `emit` was "batch-only... by design", so the standalone mirror was retracted rather than filled in. The `emit-batch` Cargo feature is unchanged in name and still gates the emitter; `emit-row`/`emit-stream` (which gated no real code) are removed.
  - `db_core::join` moves to `db_core::vm::join`. It is execution infrastructure (a hash table and join-kind emit predicate), not a parsing or planning concern, and its only consumer is `vm::batch`; available to a future `vm::row` hash join (#117) without moving again.
  - See ADR 0007's addendum for the emit rationale.

## [0.61.1] - 2026-09-06

### Fixed

- **MC/DC coverage gate restored** (db-core#111 follow-up). `tests/mcdc/obligations.json` had not been regenerated since #111; obligation ids embed line numbers, so every feature merged since orphaned its tagged vectors and `make test-mcdc` reported 42 of 70 multi-leaf obligations undischarged. Regenerated the snapshot, re-tagged the drifted vectors, and added vectors for the 13 decisions introduced by #149/#163/#167/#168/#175 that had none (`range_scan`/`index_scan` fast-path eligibility, `FROM`-subquery shape rejection and flattening, `Scope::resolve_local`'s outer-qualifier check, `GLOB ... ESCAPE` rejection, `CROSS JOIN` `LIMIT` rule, and the batch `Combine` comment). `Cargo.lock` catches up to the crate version.

## [0.61.0] - 2026-09-06

### Added

- **`codegen::row` compiles `DISTINCT`, `GROUP BY` over an arbitrary expression, `HAVING` combined with a `JOIN`, and qualified `table.*`** (#176, #177, #178, #179, split from #175, a #20 sub-ticket). `table.*` restricts the existing `*` expansion to one side of a scan/join. `GROUP BY` gains a `GroupByTarget { Column | Expr }` split mirroring `ORDER BY`'s own `OrderByTarget` (#149/#167): a non-column term compiles into an extra appended record column, keyed by index in both the sorted and hash grouping strategies (the JOIN-side grouping path stays bare-column-only, separately out of scope). `HAVING` over a joined+grouped scan reuses the same `compile_cond` evaluation the non-join path already had, against a synthetic schema spanning both sides. `DISTINCT` reuses the `ORDER BY` sorter machinery: the sort key is forced to cover every output column so duplicates land adjacent, and the drain loop skips a duplicate straight to the next sorted row (NULL-safe comparison, mirroring the aggregate module's own group-boundary check) before it can consume `OFFSET`/`LIMIT` -- composes with both. `DISTINCT` combined with `GROUP BY`/aggregation remains unsupported.

## [0.60.0] - 2026-09-06

### Changed

- **`vm::batch`'s `Opcode::Finalize` split into `Combine`/`Sort`/`Limit` sequential-phase opcodes** (#48) -- validated against DuckDB's execution model, which runs merge/finalize/`ORDER BY`/`LIMIT` as four staged operators rather than one bundled step. `Opcode::Combine{agg_parts, num_group_keys, distinct}` is the barrier (merge partial aggregates + finalize them -- no observable boundary between those two, so one opcode still covers both), with optional trailing `Opcode::Sort{col, descending}` and `Opcode::Limit{n}`. Uses the position-based split hook ADR 0007 already built for exactly this purpose -- no engine redesign. `#108`/`#109`'s bounded-scan/top-N eligibility checks re-derive their detection from the new opcode sequence, with no change to the conditions themselves. Not yet fully closing #48: column-rs's `codegen_e2e`/`oracle` test suites pin db-core via a git tag and need a coordinated follow-up bump + fixture update for the new 3-opcode AOT rendering shape.

## [0.59.0] - 2026-09-06

### Added

- **`codegen::row` compiles arbitrary expressions in SELECT-list position** (#168) -- `ProjectedColumn` (`Name`|`Expr`) replaces the SELECT-list's `Vec<String>`, mirroring the `OrderByTarget` split #167 established: a bare column keeps the existing fast path, anything else (arithmetic, function calls, scalar subqueries, `CASE`, ...) compiles through `compile_value`, reusing the same contiguity/`Copy`-consolidation and null-extension logic already in place for `ORDER BY`'s own expression columns. `index_scan`/`range_scan`'s fast paths fall back to the generic scan when any SELECT-list item is a non-bare-column expression -- a safe, no-regression fallback since such queries errored outright before. This unblocks #163's own literal example, `SELECT (SELECT x FROM t WHERE ...) FROM outer`.

## [0.58.0] - 2026-09-06

### Added

- **`codegen::row` compiles `ORDER BY` over an arbitrary expression** (#167) -- ports sqlite-rs's `OrderByTarget::Column(usize) | Expr(Expr)` split: a bare-column term still resolves to a plain `columns`-list index at plan time, but any other expression (`a + b`, `upper(name)`, etc.) now compiles into its own register via the general `compile_value` expression compiler, whose offset from the record's first register becomes its `SortKeyColumn.index`. `Opcode::SorterOpen`'s sort-key descriptor is now a patchable placeholder (`Emitter::patch_p4`), since an expression term's record position isn't known until the scan body that computes it has actually been emitted. Closes the stub #149/#166 left in place.

### Fixed

- **`codegen::row`'s `ORDER BY` expression compiler now null-substitutes a `LEFT`/`FULL` join's null-extended side** (#173) -- an `ORDER BY` expression referencing a column on a null-extended join row previously read whatever stale value that cursor's last real row left in its registers instead of `NULL`, sometimes crashing outright ("column read with no current row"). `null_extend` rewrites every such column reference into a `NULL` literal before compiling the sort expression, matching the existing null-fill behavior for a plain projected column.

## [0.57.0] - 2026-09-06

### Added

- **`codegen::row` compiles multi-term `ORDER BY`, `NULLS FIRST`/`LAST`, and expression `LIMIT`/`OFFSET`** (#149) -- threads `Vec<SortKeyColumn>` through the sort-key plumbing (the VM/opcode layer already supported multi-term sort keys via `P4::SortKey`) so every `ORDER BY` term participates in the sort, and wires `term.nulls_last` into `SortKeyColumn.nulls_first` with SQLite's default (`NULLS FIRST` for `DESC`, `NULLS LAST` for `ASC`) when unstated. `LIMIT`/`OFFSET` now compile through the general expression compiler instead of only accepting integer literals. `ORDER BY` over an arbitrary expression (not just a bare column) remains out of scope -- the current model threads extra sort columns as names, not compiled values; filed as #167.

## [0.56.0] - 2026-09-06

### Added

- **`codegen::row` compiles scalar subqueries in value position** (#163) -- adds `compile_scalar_subquery`, reusing `open_subquery_scan`/`single_result_column` from the existing `IN (SELECT ...)` machinery: scans the subquery correlated to the outer scope, applies its `WHERE` filter per row, stops at the first matching row and copies its single projected column into a register, or loads `NULL` if the scan exhausts with no match. More than one column is rejected at compile time via the same `single_result_column` check `IN (SELECT ...)` uses. `SELECT`-list expression projection (needed for the ticket's own literal example, `SELECT (SELECT x FROM t WHERE ...) FROM outer`) remains a separate, pre-existing limitation -- filed as #168.

## [0.55.0] - 2026-09-06

### Added

- **`codegen::row` compiles bind parameters** (#162) -- `?`/`?NNN`/`:name`/`@name`/`$name` all compile via `Opcode::Variable` (VM-side execution already existed). `RegAlloc` tracks per-compile parameter slots: `?`/`?NNN` get positional slots (a numbered claim bumps the anonymous counter past itself, matching SQLite); `:name`/`@name`/`$name` get a slot on first occurrence and reuse it on repeat within the same statement. `Program` exposes `param_names` (slot -> optional name) so a caller can bind by name; positional binding continues via the existing `Vm::bind_params`. `:foo`/`@foo`/`$foo` are treated as three distinct named parameters (keyed by sigil+name), not aliases sharing one slot -- a conservative reading pending confirmation against real SQLite's cross-sigil behavior.

## [0.54.0] - 2026-09-06

### Added

- **`codegen::row` compiles `IS`/`IS NOT`, `BETWEEN`, `IN (list)`, `LIKE`/`GLOB`, `CASE`, `CAST`, `COLLATE`, and general scalar function calls** (#150) -- constructs `parser::ast` could express since #147/#152's retarget but `codegen::row` still returned `Unsupported` for. `IS`/`BETWEEN`/`IN` are ported directly from sqlite-rs's own `codegen/expr/cond.rs` (single evaluation of the tested expression; `NOT BETWEEN`/`NOT IN` correctly distinguish a definite non-match from an unknown one via a `saw_null` register, rather than a naive true/false swap). `LIKE`/`GLOB` and general function calls dispatch into `vm::row::functions`' existing registry (`upper`/`substr`/`coalesce`/`like`/`glob`/...) via `Opcode::Function` -- no VM changes needed. An explicit `expr COLLATE name` on a comparison operand now selects the real collation (`BINARY`/`NOCASE`/`RTRIM`) instead of always defaulting to `Binary`; an unrecognized name is rejected rather than silently ignored. Validated against a real `sqlite3` CLI oracle (`examples/oracle_check.rs`, kept as a manual dev tool): 24/24 match, including `IN`/`BETWEEN`'s NULL-propagation edge cases. Bind parameters (#162) and scalar subqueries in value position (#163) remain `Unsupported`, split into their own tickets since they need new VM/codegen machinery rather than mechanical wiring.

## [0.53.0] - 2026-09-06

### Added

- **`codegen::row` non-recursive `WITH`-clause / CTE support** (#143) -- ports sqlite-rs's `src/codegen/subquery/cte.rs` into `codegen::row::subquery::cte`, near-verbatim: `parser::ast` is sqlite-rs's own AST post-#147/#153, so this needed no AST-level scoping down. `expand_with_clause` rewrites every `FROM`/`JOIN` table reference naming a CTE into a `TableRefKind::Subquery` wrapping that CTE's query, reusing the existing FROM-subquery materialization machinery -- handles a later CTE referencing an earlier one, an inline derived table referencing a CTE in its own `FROM`, and an explicit `WITH cte(a, b) AS (...)` column-rename list. Hooked into `compile_select_with_catalog` ahead of predicate pushdown/flattening, so a flattenable CTE body never pays for an ephemeral table. `RegAlloc` grows a structural-equality `cte_cache` (`cached_cte`/`cache_cte`), and `materialize_from_subquery` `OpenDup`-reuses a structurally identical subquery instead of re-materializing it. `WITH RECURSIVE` remains unsupported, mirroring the parser's own rejection.

## [0.52.1] - 2026-09-06

### Changed

- **`src/` is in the `cargo-mvl-limit` qualified subset, and the gate is blocking** (#156, #161) -- all 101 violations the gate reported when the CI pipeline landed (#157) are cleared: no explicit lifetimes, no `dyn` dispatch, no `unreachable!`/`env!` outside the designated boundary. `vm/row`'s `Cursor`/`Transaction`/`CursorFactory`/`SchemaStorage` remain `Box<dyn ..>` as ADR 0008's storage-agnostic extension point for downstream implementors, exempt via the Makefile's `MVL_LIMIT_EXCLUDE` -- the same convention sqlite-rs applies to its `src/vfs.rs`.
- **Batch execution API is generic instead of boxed** -- `vm::engine::run`, `vm::batch::run_parallel`/`run_parallel_top_n` take `&[S: Segment]` (was `&[Box<dyn Segment>]`); `Vm::run` takes `&mut S: Source`; `codegen::batch::explain` takes `impl Fn(&str) -> TableStats`. Every call site was already monomorphic, so callers drop the boxing and `as Box<dyn Segment>` casts. Downstream adaptation tracked in column-rs#18.
- **`SemiJoinProgram` owns its `subquery: Box<Select>`** (was a borrowed `&'q Select`), and `JoinHashTable::get_all` returns an eager `Vec<&V>` (was a lazy iterator).
- **`VmError::UnsupportedOp`** -- a `Map`/`Window` opcode reaching a kernel with no dispatch for it is now a returned error for the caller to handle, not a panic mid-query. `emit::batch` renders a `compile_error!` into generated source for opcode/expression shapes no planner emits yet, instead of panicking in the emitter.
- **Crate version is `db_core::VERSION`** (a plain constant; `env!` is outside the qualified subset). `tests/version.rs` fails the build if it drifts from `Cargo.toml` -- bump both on release.

### Fixed

- ADR 0002 referenced `convert_select` by its pre-#153 name (#160).

## [0.52.0] - 2026-09-06

### Added

- **`codegen::row`'s SELECT/DML planner is wired into `dispatch::compile_statement`** (#148) -- `SELECT` (including a `WITH`-prefixed one), `INSERT`, `UPDATE`, and `DELETE` are now routed and compiled end to end, alongside the existing DDL/`PRAGMA`/transaction statements. `codegen::row` goes from DDL-only to actually executing queries. A `SELECT` with a single `JOIN` resolves its right-hand table from the schema catalog and compiles via `compile_select_join`; anything else goes through `compile_select_with_catalog`. Unknown-table errors are reported consistently as `DispatchError::NoSuchTable` across every statement kind.

## [0.51.0] - 2026-09-05

### Added

- **`codegen::row` aggregation, index/range/limit scans, and subqueries** (#93, #94, #95 -- #20 sub-tickets) -- ports `codegen::row::aggregate` (GROUP BY sorted/hash strategies per ADR 0032, HAVING, aggregate-over-join, `AggStep` p4 collation/p5 reset), index-ordered scans (ADR 0020), index range seeks (ADR 0034), LIMIT/OFFSET fast paths, EXPLAIN QUERY PLAN, and scalar/`EXISTS`/`IN`/FROM-clause subquery codegen, mechanically from Lab271/sqlite-rs, targeting `db_core::vm::row::Opcode` directly (the #18 decision). Adds `Query.having`, `Query.offset`, `Expr::Exists`, and turns `Query.from` into a `FromClause` (`Table`/`Subquery`) enum -- minimal, additive `db_core::expr` AST changes mirroring sqlite-rs's own AST shape. Skip-scan/full cost-based scan chooser (#144) and CTE support (#143) are deliberately deferred as follow-ups.

### Fixed

- **`codegen-row` feature now implies `parser-row`** -- `cargo build --no-default-features --features codegen-row` previously failed since some `codegen/row/*` files (from #97) depend on `parser-row`, which wasn't pulled in transitively.

## [0.44.2] - 2026-09-05

### Fixed

- **`ORDER BY` may reference a `SELECT`-list aggregate** (#131) -- `ORDER BY COUNT(x) DESC` previously failed with `expected a column reference, found FunctionCall {...}` because `parser::column`'s `ORDER BY` lowering only accepted a bare/qualified column name. `AggFunc::name()` is now the single source of truth for an aggregate's canonical label, shared by `codegen::batch`'s `SELECT`-list rendering and the new `ORDER BY` lowering, which accepts an aggregate call (no `DISTINCT`, no `OVER`) matching a known `AggFunc` and renders it to the identical label a matching `SelectItem::Agg` produces; `codegen::batch::select_output_index` now resolves `ORDER BY` against any `SELECT`-list item's rendered label, not just `SelectItem::Column`. Arbitrary non-aggregate expressions, ordinal-position `ORDER BY N`, and multi-key `ORDER BY` remain out of scope, left open on #131.

## [0.44.0] - 2026-09-05

### Added

- **`vm::row` cursor factory, DDL/ANALYZE schema hook, auto-index/misc opcodes, and index-mode cursors** (#125, #128, #127, #126 -- #18 sub-tickets) -- closes out `vm::row`'s remaining `ExecError::Unimplemented` opcodes. `CursorFactory` + `Vm::set_cursor_factory` resolve `OpenRead`/`OpenWrite`'s `p2` root page through a consumer hook, replacing the old pre-wired-only assertion (`OpenDup`/`OpenPseudo` implemented alongside). `SchemaStorage` + `Vm::set_schema_storage` drive `CreateTable`/`CreateIndex`/`DropTable`/`DropIndex`/`Analyze` from a storage-agnostic hook (ADR 0008). `AutoIndexInsert`/`Seek`/`Rowid`/`Next` (an in-memory transient join index), `Count` (with a `Cursor::count()` fast-path/scan-fallback), `Last`, `NullRow`, and `Sequence` are now dispatched. The `Cursor` trait gains an index-mode contract (`seek_index_eq`/`seek_index_ge`/`idx_compare`/`idx_rowid`) backing all eleven index-cursor opcodes (`IdxRewind`/`Last`/`Next`/`Prev`/`Rowid`, `SeekIndexEq`/`GE`, `IdxCompareGT`/`LE`, `Found`, `NoConflict`), with an `InMemoryIndexCursor` fixture and published conformance checks for a real adapter to reuse.

## [0.43.0] - 2026-09-05

### Added

- **`vm::row`'s `Cursor` trait gains `seek`/`payload`, plus a `Transaction` hook and a public conformance suite** (#81) -- `Cursor::seek(rowid)` wires `Opcode::SeekRowid`'s previously-unimplemented dispatch to direct key-based positioning (overridden by `InMemoryCursor`/`EphemeralTableCursor`); `Cursor::payload()` is a lazy raw-record-bytes hook for a real b-tree cursor, proven sufficient by a new `MockTableCursor` test type (no production cursor in this crate retains raw bytes to override it with). `vm::row::transaction::Transaction` is a `begin`/`commit`/`rollback` hook a consumer's pager installs via `Vm::set_transaction_hook`, finally giving `Opcode::Transaction`/`AutoCommit` real dispatch arms that toggle `Vm::autocommit` and, when installed, drive the hook. `vm::row::cursor_conformance` publishes trait-level `Cursor` conformance checks so a real adapter (t-rust-db/sqlite-rs) can run the same checks this crate runs against its own fixtures, without `db-core` ever depending on `db-storage`. ADR 0008 amended: the adapter's location (sqlite-rs, not a `db-storage` feature) is now a decided fact.

## [0.42.0] - 2026-09-05

### Added

- **`vm::row` remaining scalar functions** (#90) -- closes the gap against sqlite-rs's `vdbe::functions` entirely: `substr`, `trim`/`ltrim`/`rtrim`, `replace`, and `like`/`glob` with their recursive pattern matchers (`like_match`/`glob_match`, exposed for a future `LIKE`/`GLOB` operator). Confirmed `vdbe::result`/`vdbe::arithmetic` needed no porting -- every opcode they back is already dispatched in `vm.rs`.

## [0.41.0] - 2026-09-05

### Added

- **`vm::row` `EXPLAIN` opcode-listing rendering** (#88) -- ports sqlite-rs's `vdbe/explain.rs`: `explain(&Program) -> Vec<ExplainRow>`, one row per instruction (`addr`/`opcode`/`p1..p5`/`p4`-in-display-form/`comment`), with `opcode_name`/`render_p4` exhaustive over `vm::row`'s actual `Opcode`/`P4` shapes. Prefers an instruction's own `comment` field (ADR 0007) over the computed fallback, so a codegen-supplied comment is never discarded. `EXPLAIN QUERY PLAN`'s tree renderer is out of scope -- it lives in the query planner, not the VDBE.

## [0.40.0] - 2026-09-05

### Added

- **`codegen::row` gains `FULL OUTER` join emission** (#101, scoped) -- `compile_select_join` now supports `FULL OUTER` via a two-pass nested loop: pass one reuses `LEFT`'s matched/null-extend loop (left-outer, right-inner); pass two mirrors it (right-outer, left-inner), emitting a left-null-extended row for every right row pass one's inner loop never matched. `LIMIT`'s guard targets a distinct final label when the join is `FULL`, so hitting the limit during pass one correctly skips pass two. The rest of #101 as originally scoped -- a real `planner::Stats` cost model and the join-order/access-path chooser, N-way joins and a multi-table catalog `Scope` -- is deferred: `ANALYZE` has no working VM implementation yet (filed as #116, a bug -- #97 shipped codegen for an opcode the VM can't execute, so there's no real data a cost model could read), and no consumer anywhere needs N-way joins, matching #97's own precedent against vendoring speculative infrastructure with no caller (#117/#118 track the follow-ups).

## [0.39.0] - 2026-09-05

### Added

- **`vm::row` PRAGMA opcodes `SetJournalMode`/`Synchronous`** (#89) -- ports the two `vdbe/pragma.rs` functions that have a well-defined no-writer fallback in sqlite-rs itself; `db-core` has no pager (ADR 0008/0006), so its `Vm` is always in that "read-only connection, no writer attached" state. `SetJournalMode` is unconditionally a no-op but errors with a new `ExecError::JournalModeChangeDuringTransaction` if a new `Vm::autocommit` flag (default `true`, ahead of #81's full transaction-hook surface) is `false`. `Synchronous`'s bare query form always reports `FULL`; the set form is a no-op. `IntegrityCheck` stays unimplemented -- it has no no-writer fallback in sqlite-rs, always needing a real page source `db-core` has no concept of.

## [0.38.0] - 2026-09-05

### Added

- **`vm::row` `GROUP BY` hash aggregation** (#86) -- ports sqlite-rs's hash-aggregation opcode family (`HashAggOpen`/`Find`/`Step`/`Rewind`/`Data`/`Next`, `P4::GroupKey`) as the O(n) alternative to `SorterCursor`'s sort-then-group strategy, backed by a new `cursor::HashAggCursor`: `HashAggFind` locates (creating on first sight) a row's group by its decoded key columns, retaining only the group's first row; `HashAggStep` folds into the current group's own accumulator slots; `HashAggRewind` freezes and orders groups by key; `HashAggData` installs the current group's accumulators into `Vm`'s `agg_contexts` table so the existing `AggFinal` opcode needs no hash-specific case. Group lookup is a documented linear scan rather than an actual hash table -- a correctness-equivalent simplification for this non-perf-critical reference VM.

## [0.37.0] - 2026-09-05

### Added

- **`vm::row` multi-key and `LIMIT`-bounded sorter** (#87) -- `cursor::SorterCursor` extends from single-key to multi-key `ORDER BY` (`P4::SortKey` now carries `Vec<SortKeyColumn>`, compared left-to-right) and an optional top-K bound driven by `SorterOpen`'s `P5`/`P2` (mirroring sqlite-rs's `OffsetLimit`-derived `LIMIT` bound): once the buffer exceeds the bound, it is re-sorted and truncated to keep only the best-so-far rows -- a correctness-equivalent, simpler stand-in for sqlite-rs's heap-ordered eviction.

## [0.36.0] - 2026-09-05

### Added

- **`codegen::row` gains a single equi-join and `ORDER BY`** (#102) -- `compile_select_join` adds a single `INNER`/`LEFT` nested-loop equi-join (`LEFT`'s null-extension via `Opcode::Null` for unmatched outer rows) and `ORDER BY` buffered through the existing single-key sorter opcodes (`SorterOpen`/`Insert`/`Sort`/`Next`), decoded back via `Opcode::Column` against the sorter cursor; `LIMIT` now applies to the sorted output rather than scan order when `ORDER BY` is present. `Scope` grows an optional right-hand table binding, resolved with `codegen::batch::compile_join`'s already-shipped qualified-column convention (unqualified name resolves to the left/`FROM` table only). `Right`/`Full`/`Cross` joins, N-way joins, and the join-order/access-path chooser stay deferred to #101 (needs `planner::Stats`, which db-core doesn't have yet).

## [0.35.0] - 2026-09-05

### Added

- **`db_core::value`** (#83) -- `Value`/`Collation`/`TextEncoding`/`compare_text`/`format_real` move to a feature-free, dependency-free crate-level module per ADR 0010; `vm::row::value` re-exports the same items so every existing `super::value` path inside `vm::row` is unchanged. Landed alongside db-storage's matching PR (db-storage#18, v0.5.0), which now re-exports `db_core::value::Value` as `db_storage::row::record::Value` -- exactly one `Value`/`Collation` definition across both crates.

## [0.34.0] - 2026-09-05

### Added

- **`codegen::row` gains `INSERT`/`UPDATE`/`DELETE` and secondary-index maintenance** (#96) -- ports sqlite-rs's `codegen/stmt.rs` + `codegen/stmt/{insert,update,delete}.rs` + `codegen/index_maintenance.rs`, scoped down like #91/#92's precedent: single-table only, no `INSERT ... SELECT`/`ON CONFLICT`/upsert/`RETURNING`, no `UPDATE ... FROM` or rowid-alias reassignment, no `DELETE` rowid-seek fast path (full scan only, deferred alongside #94's index-scan codegen). Adds `crate::expr::{Insert, Assignment, Update, Delete}` and extends `TableSchema`/`IndexSchema` with the column lists index maintenance needs. Also implements the write-path VM opcodes these programs execute (`Delete`/`NewRowid`/`IdxInsert`/`IdxDelete`), previously listed in `vm::row::Opcode` but unimplemented, and reworks `EphemeralTableCursor` to position by rowid (kept sorted) instead of `Vec` index so `DELETE`/`UPDATE`'s mid-scan cursor mutation can't skip or revisit rows.

## [0.33.0] - 2026-09-05

### Added

- **`codegen::row` gains DDL, transactions, PRAGMA, ANALYZE and statement dispatch** (#97) -- ports sqlite-rs's `src/codegen/{ddl,transaction,pragma,analyze,dispatch}.rs`: `CREATE`/`DROP TABLE`/`INDEX`/`CREATE VIEW` each compile to a single procedural opcode carrying its `sqlite_master` payload in `P4`; `BEGIN`/`COMMIT`/`ROLLBACK` compile to `Transaction`/`AutoCommit`; `PRAGMA journal_mode`/`integrity_check`/`quick_check`/`synchronous` compile to their matching control opcodes; `ANALYZE` bakes every target table's (and its indexes') root pages into `P4::Analyze` for the exec-time `sqlite_stat1` rewrite. `codegen::row::dispatch::compile_statement` keyword-sniffs a raw SQL string and routes it to the matching compiler against a schema catalog, scoped to the statement kinds this crate has codegen for today (DDL/transaction/PRAGMA/ANALYZE plus #91's expressions) -- `INSERT`/`UPDATE`/`DELETE`/`SELECT` dispatch is deferred to whichever sub-ticket of #20 ports their codegen. `TableSchema` gains `root_page`/`indexes` (a new placeholder `IndexSchema`) to bake catalog identity into `P4` at codegen time. `vm::row::Opcode`'s DDL/PRAGMA/transaction opcodes gain their `P4` payload shapes and `TRANSACTION_MODE_*`/`JOURNAL_MODE_*`/`SYNCHRONOUS_*` constants. Porting sqlite-rs's `src/planner.rs` cost model (also part of #97) is deferred as a follow-up: it has no caller yet without real storage integration (#18) and join codegen (#92).

## [0.32.0] - 2026-09-05

### Added

- **`codegen::row` gains single-table `SELECT` compilation** (#92) -- `compile_select` covers a single-table scan (`Rewind`/`Next`) plus projection (bare columns, `SELECT *` expansion) plus `WHERE` filtering plus `LIMIT` (ported 1:1 from sqlite-rs's `IfNotZero`/`Goto` guard, including the `LIMIT 0` check-before-emit ordering). Joins and `ORDER BY` are deferred to #102; the join-order/access-path chooser and `FULL OUTER` to #101 (needs `planner::Stats`, which db-core doesn't have yet); `GROUP BY`/aggregation to #93.

## [0.31.0] - 2026-09-05

### Added

- **`codegen::row` gains expression compilation** (#91) -- ports sqlite-rs's `src/codegen.rs` shared infra + `expr.rs`/`expr/{cond,value}.rs`, targeting `vm::row::Opcode` directly, scoped to what db-core's current `Expr`/`Opcode` support today: a minimal `Emitter`/`RegAlloc`/`Scope`/`CondTargets` jump-based compilation mechanism covering `Not`, `And`/`Or` (cheapest-first short-circuit reordering), comparisons, `IsNull`, arithmetic, `Concat`, and column/literal reads, plus a 200-level expression-depth bound. `InSubquery` codegen and `Case`/`Cast`/`Like`/`Between`/`Exists`/multi-table `Scope` are deferred to later sub-tickets (#92/#95) that actually need them. `codegen-row` now depends on `vm-row`.

## [0.30.0] - 2026-09-05

### Fixed

- **`SELECT` mixing a bare column with an aggregate crashed at runtime instead of being rejected** (#99) -- `SELECT active, MAX(id) FROM t` (no `GROUP BY`) compiled the bare column as a full-length register and the aggregate as a single collapsed value; `Opcode::Emit` then indexed both as if the same length and panicked ("index out of bounds") once it reached the aggregate's. A related, non-crashing bug also existed with `GROUP BY` present: `compile` emits non-aggregated SELECT columns via `GROUP BY`'s own key registers in `GROUP BY`'s stated order, never checking each SELECT column against `group_by` -- so a column that wasn't a group key, or `GROUP BY` keys listed in a different order than `GROUP BY` itself, silently produced wrong/misaligned results. Both are now rejected at parse time (`parser::column::convert_select`): every non-aggregated SELECT column must appear, in the same order, as `GROUP BY`. Window functions remain exempt (no `GROUP BY` requirement).

## [0.29.0] - 2026-09-05

### Added

- **`vm::row::Cursor` gains `prev`/`last`/`delete`** (#76), matching the shape a real db-storage-backed cursor will need. `InMemoryCursor` and `EphemeralTableCursor` implement them; defaults preserve prior behavior for other cursor kinds.
- **`Opcode::OpenRead`/`OpenWrite` are now dispatched** (#76) -- previously unhandled in the exec loop. For now this only asserts the cursor slot was pre-wired via `Vm::open_cursor`; real root-page/pager semantics against `db-storage` remain blocked on t-rust-db/db-storage#8, which adds the read-only `TableCursor` this trait's eventual adapter will wrap.

## [0.28.1] - 2026-09-05

### Changed

- **`vm::batch`'s `run_parallel`/`run_parallel_top_n` no longer depend on `rayon`** (#42) -- replaced `par_iter().map().collect()` with a std-only morsel-driven fork-join: `std::thread::scope` spawns a fixed pool of worker threads that dynamically pull segment indices off a shared `AtomicUsize` counter, preserving the "one segment per task, dynamically rebalanced" scheduling model the code's own doc comments named as a deliberate DuckDB/HyPer-style design goal (not a static per-thread split). `rayon`/`rayon-core`/`crossbeam-deque`/`crossbeam-epoch`/`crossbeam-utils`/`either` are dropped from `Cargo.toml`; `cargo tree` now shows zero third-party runtime dependencies.

## [0.28.0] - 2026-09-05

### Added

- **Window function parsing** (#74 follow-up) -- `parser::row`'s grammar parses `func(...) OVER (PARTITION BY ... ORDER BY ...)` (a new `ast::WindowDef`, `ExprKind::FunctionCall`'s new `over` field, `Parser::window_def`), and `parser::column` lowers it into `SelectItem::Window` via the new `WindowFunc::from_name`/`convert_window_call`, resolving all 10 `WindowFunc` variants (`ROW_NUMBER`/`RANK`/`DENSE_RANK`, `LAG`/`LEAD` with an optional integer offset, `FIRST_VALUE`/`LAST_VALUE`, `SUM`/`AVG`/`COUNT` as windowed aggregates) with per-kind argument validation (niladic for the ranking functions, one column or `COUNT(*)` for the rest). Sqlite-rs has no prior art for this (same unimplemented stub), so the grammar was designed fresh against SQLite's/DuckDB's actual window-function syntax rather than ported.
- Deliberately **not** carried forward, each rejected with a specific "not yet supported" error rather than silently accepted or misconverted: a named `OVER window_name` (would need the still-unsupported `WINDOW` clause), an explicit frame (`ROWS`/`RANGE`/`GROUPS BETWEEN ...` -- `WindowSpec` has no frame representation; every window function runs over its fixed default frame instead), and `FILTER (WHERE ...)` on an aggregate/window function.

## [0.27.0] - 2026-09-05

### Fixed

- **`KEY` is no longer a reserved keyword token** (#71 follow-up) -- the parser unification (#57) made `parser::column` a thin adapter over `parser::row`'s shared tokenizer/grammar, which reserved `KEY` (needed only for `PRIMARY KEY` DDL syntax) as a token in its own right. Since this tokenizer has no LALR-style keyword-as-identifier fallback (unlike real SQLite), that made `KEY` unusable as an ordinary column name anywhere the shared grammar is used -- e.g. `SELECT ... FROM orders JOIN regions ON orders.region_key = regions.key` failed with `expected identifier, found Keyword(KEY)`. `KEY` is now tokenized as a plain identifier; `PRIMARY KEY` parsing matches it case-insensitively via the new `Parser::expect_bareword_ci` instead of a dedicated keyword token.

## [0.26.0] - 2026-09-05

### Added

- **`vm::row`'s minimal single-key sorter** (#69, follow-up to #59/#18). Ports `SorterOpen`/`SorterInsert`/`SorterSort`/`Sort`/`SorterNext`/`SorterData` via a new `SorterCursor` that implements the existing `Cursor` trait (`rewind()` sorts and positions at the first row, doubling as `SorterSort`'s dispatch target). Adds `program::SortKeyColumn` and `P4::SortKey`. **Single-key, no LIMIT/bound** -- multi-key sort and bounded top-K maintenance remain follow-ups.

## [0.25.0] - 2026-09-05

### Fixed

- **`EXPLAIN QUERY PLAN` now shows a `DISTINCT` node** (#71) -- `codegen::batch::explain` threaded `query.distinct` into the compiled program (`Opcode::Finalize`'s `distinct` field) and the VM really deduped, but the plan tree never reflected it, silently under-reporting what the query does. The node appears after `GROUP BY`/`AGGREGATE` and before `ORDER BY`/`LIMIT`, matching `compile`'s actual dedup ordering (post-`Finalize`, pre-sort/limit).

## [0.24.0] - 2026-09-05

### Added

- **`vm::row`'s scalar function set, second slice** (#68, follow-up to #64/#18). Adds `sign`, `zeroblob`, `iif`, scalar `min`/`max`, `sqlite_version`, `round`, `hex`, `unhex`, `instr`, `quote` to `vm::row::functions`'s registry -- no new opcode wiring needed, `Opcode::Function` already dispatches generically by name/arity. `like`/`glob` and the `substr`/`trim`/`replace` family remain deferred.

## [0.23.0] - 2026-09-04

### Added

- **`vm::row`'s scalar function set, first slice** (#64, follow-up to #62/#18). Ports sqlite-rs's `vdbe::functions` dispatch pattern into `vm::row::functions`: `abs`/`length`/`upper`/`lower`/`coalesce`/`ifnull`/`nullif`/`typeof`, wired to `Opcode::Function` (reusing `AggFinal`'s `P4::Str("name(arity)")` descriptor convention). `like`/`glob` and the `substr`/`replace`/`trim` family remain deferred.

## [0.22.0] - 2026-09-04

### Added

- **`vm::row`'s aggregate accumulators (`AggStep`/`AggFinal`), single-group only** (#62, follow-up to #59/#18). Ports sqlite-rs's `vdbe::aggregate` (`COUNT`/`SUM`/`AVG`/`MIN`/`MAX`) into `vm::row::aggregate`. Adds `P4::AggFunc { name, arity, collation }` and an `agg_contexts` slot table on `Vm` (mirrors `cursors`). `GROUP BY` hash aggregation (`hash_agg.rs`) remains deferred; this buys `SELECT COUNT(*), SUM(x) FROM t`-shaped queries, proven end-to-end against #59's `EphemeralTableCursor` scan.

## [0.21.0] - 2026-09-04

### Added

- **Bare `EXPLAIN` opcode listing** (#55). `parser::column::parse_explain` now returns a 3-variant `Explain` enum (`None`/`Opcodes`/`QueryPlan`) instead of a bool, so callers can distinguish bare `EXPLAIN` from `EXPLAIN QUERY PLAN` (previously collapsed into one flag). New `codegen::batch::explain_opcodes(query)` builds a bare `EXPLAIN`'s opcode listing: one `OpcodeSection` per phase the executor actually runs (flat/GROUP BY/ORDER BY/LIMIT -> one `body`; join -> `build`/`probe`/`body`; semi-join and window -> one `body`), each holding `OpcodeRow`s (`addr | opcode | operands | comment`) with named-field operands (not a `Debug` dump) and the `Finalize` barrier flagged via `Program::split_finalize`. `Opcode::name()` is now `pub`.

## [0.20.0] - 2026-09-04

### Added

- **`vm::row`'s record decoding, plus `Opcode::OpenEphemeral`/`Insert` over an in-memory ephemeral table** (#59, follow-up to #56/#18). Ports sqlite-rs's `record::decode` (`decode_record`/`decode_column`) into `vm::row::record`, self-contained like encode. `Cursor` gains an `insert(&mut self, rowid, values) -> bool` method (default `false`); `EphemeralTableCursor` is a real, `Insert`-writable in-memory table (rows carry an explicit caller-assigned rowid). `Opcode::Insert` decodes `MakeRecord`'s blob straight back into `Value`s before storing, matching sqlite-rs's "decode-once-at-insert" design. This is the first slice where a hand-built `Program` runs a genuinely complete end-to-end micro-query: `MakeRecord` -> `Insert` -> `Rewind`/`Next` (scan) -> `Column` -> `ResultRow`, entirely storage-agnostic.

## [0.19.0] - 2026-09-04

### Added

- **`vm::row`'s on-disk record encoding, wired to `Opcode::MakeRecord`** (#56, follow-up to #51/#18). Ports sqlite-rs's `record::encode` (varint writer + `encode_record`) into `vm::row::record`: a varint header length, one varint serial type per column (smallest-lossless-width integers, `12+2*len`/`13+2*len` for BLOB/TEXT, 8-byte IEEE-754 for REAL), then column bodies back-to-back. `MakeRecord` packs a contiguous register range into a record blob, applying a `P4::Affinity` byte string per-column to a copy before encoding. Record decoding and ephemeral-cursor wiring remain follow-up work.

## [0.18.0] - 2026-09-04

### Added

- **`vm::row`'s fetch-decode-execute loop, control flow, and a storage-agnostic cursor trait** (#51, follow-up to #18). Ported from sqlite-rs's `exec.rs`/`control.rs`/`arithmetic.rs`/`result.rs`: the register file + `vm::row::vm::execute`, control flow (`Init`/`Goto`/`Once`/`BeginSubrtn`/`Return`/`Halt`/`IfNot`/`IfNotZero`/`IfPos`/`DecrJumpZero`/`IsNull`/`NotNull`/`MustBeInt`/`OffsetLimit`), fused compare-jump (`Eq`/`Ge`/`Gt`/`Le`/`Lt`), `RealAffinity`/`Cast`, arithmetic (`Add`/`Subtract`/`Multiply`/`Divide`/`Remainder`/`Not`/`BitAnd`/`BitOr`/`ShiftLeft`/`ShiftRight`/`BitNot`/`Concat`), result-row loads (`Integer`/`Int64`/`Real`/`Blob`/`Null`/`String8`/`Variable`/`Copy`/`ResultRow` -- `MakeRecord`'s record encoding deferred), and a storage-agnostic `Cursor` trait (`Rewind`/`Next`/`Column`/`Rowid`) with an `InMemoryCursor` mock proving the row-at-a-time model end-to-end.

### Fixed

- **ADR 0008 corrected**: `vm::row::Opcode`/`Instruction` had mistakenly generalized `vm::batch`'s typed-operand design (ADR 0007) to `vm::row`, which is meant to be a literal, opcode-for-opcode port of sqlite-rs's VDBE. `Opcode` is now a bare tag enum listing every sqlite-rs VDBE variant; `Instruction` carries sqlite-rs's literal `p1/p2/p3/p4/p5` operands. Breaking change to #18's just-landed `Opcode` shape (`Compare`/`Cast`/`Arith`/`Logic`/`Not`/`BitNot`/`Neg`).

## [0.17.0] - 2026-09-04

### Fixed

- **`emit::batch`'s generated `Query { ... }` literal was missing the `distinct` field** added by `SELECT DISTINCT` (#47, v0.14.0), so any codegen'd binary for a join/semi-join/windowed query failed to compile with `E0063: missing field 'distinct'` -- `render_query` never threaded `query.distinct` through to the emitted source. Found via column-rs's `codegen_e2e` test suite (t-rust-db/column-rs#9).

## [0.16.0] - 2026-09-04

### Added

- **`vm::row`'s value-semantics slice** (#18, ADR 0008) -- the first real content in `db_core::vm::row`, ported from sqlite-rs's VDBE with zero I/O/storage coupling: `value` (`Value`/`Collation`/`compare_text`/`format_real`), `compare` (cross-type ordering: NULL < numeric < text < blob), `logic` (three-valued logic / NULL propagation), `affinity` (column type affinity), `cast` (`CAST` conversion), `coerce` (text-to-numeric coercion and checked arithmetic, overflow promotes to REAL). A partial `Opcode`/`Instruction`/`Program` skeleton covers just these ops; the execution loop, cursor trait, and remaining opcodes are tracked in #51.
- **ADR 0008** resolves `vm::row`'s two open design questions: `Opcode` is a mechanical port of sqlite-rs's VDBE opcode set (typed operands, following ADR 0007's precedent for `batch`), and the eventual cursor abstraction will be a storage-agnostic trait rather than a direct `db-core` -> `db-storage` dependency.

## [0.15.0] - 2026-09-04

### Added

- **`codegen::batch::expand_star(query, schema)`** (#46) -- resolves `SelectItem::Star` against a caller-supplied schema (column names in order), replacing it with `SelectItem::Column` entries; mixed `SELECT id, * FROM t` keeps `id` first. `db-core` has no Parquet/schema access itself, so this is meant to be called once by the schema-aware caller (e.g. column-rs's `QueryEngine`) before `compile`/`compile_join`/`compile_semi_join`/`compile_window`. A query with no `Star` is returned unchanged.
- **`PlanError::StarWithAggregation`** -- returned by `expand_star` when `*` is combined with `GROUP BY` or an aggregate/window select item, since the row shape is no longer well-defined to expand `*` against.

## [0.14.0] - 2026-09-04

### Added

- **`SELECT DISTINCT`** support in column-rs's grammar (#47), including `DISTINCT` combined with `GROUP BY` (dedup applied after the GROUP BY hash-aggregate merge, matching DuckDB's semantics). No new opcode: `distinct: bool` is threaded through `expr::Query` and the terminal `Opcode::Finalize`; deduplication runs in `vm::engine::finalize` as a stable pass over the fully materialized cross-segment output, after the GROUP BY merge and before `ORDER BY`/`LIMIT`.

### Changed

- `vm::engine::bounded_scan_limit` and the `ORDER BY`/`LIMIT` top-N fast path in `run` now fall back to the general (fully materializing) path when `distinct` is set, since both bypass the full materialization DISTINCT's dedup pass needs before sort/limit can run correctly.

## [0.13.0] - 2026-09-04

### Added

- **`vm::batch::Program`/`Instruction`** mirroring sqlite-rs's `vdbe::program` shape (ADR 0007): a `Program` is a `Vec<Instruction>`, each `Instruction` a typed `Opcode` plus an optional `EXPLAIN` comment. Operands stay typed and named on the `Opcode` enum (not sqlite-rs's `p1..p5` integer slots). `Program::columns_to_load()` derives the input columns from the `LoadColumn` instructions; `Program::split_finalize()` separates the body from the terminal `Finalize`.
- **`Opcode::Finalize { agg_parts, num_group_keys, order_by, limit }`** -- the terminal opcode of a planned flat program, carrying the cross-segment merge/`ORDER BY`/`LIMIT` metadata that column-rs's `Plan` used to hold as sidecar fields. The per-segment `Vm` treats it as a no-op control opcode (like `Scan`/`Halt`); `vm::engine::run` applies it once over the concatenated per-segment output. `AggPart` moves here from column-rs.
- **`vm::engine`** (gated with `vm-batch`): `run(segments, &Program)` -- body per segment via `run_parallel`/`run_parallel_top_n` (or a sequential bounded scan for a bare `LIMIT`), then `Finalize` once; `finalize()` (column-rs's former `query::post_process`, moved unchanged); `run_join`/`JoinProgram` (the `HashBuild`/`HashProbe` two-phase driver, from `execute_joined`); `semi_filter`; `InMemorySegment`.

- **`codegen::batch`** -- the columnar planner, moved from column-rs's `src/query.rs` (it never touched Parquet): `compile(&Query) -> Program` (the former `Plan` struct is gone -- its `columns_to_load` is `Program::columns_to_load()`, its `agg_parts`/`num_group_keys`/`order_by`/`limit` are the terminal `Opcode::Finalize`), `compile_join -> JoinProgram`, `compile_semi_join -> SemiJoinProgram`, `compile_window -> Program` (window queries are now an ordinary flat program: `LoadColumn`s, `Window`s, `Emit`, `Finalize`), `output_column_names`, `PlanError`, and the `EXPLAIN` plan tree (`explain(&Query, &dyn Fn(&str) -> TableStats) -> Vec<PlanNode>`; storage supplies only each table's row-group/row counts). Instructions carry `EXPLAIN` comments (`r0 = region`, `WHERE id > 1000`, `GROUP BY region`, ...).
- **`emit::batch::generate(crate_name, sql)`** -- the end-to-end "SQL text to `.rs` source" entry point, moved from column-rs's `src/codegen.rs` (which is deleted there). `render_flat` now takes the planned `&Program` and emits it whole (including `Finalize`, with instruction comments as trailing `//` comments); the generated binary calls `{crate_name}::query::run_program(&file, PROGRAM)` -- no more `COLUMNS_TO_LOAD`/`AGG_PARTS`/`NUM_GROUP_KEYS`/`ORDER_BY`/`LIMIT` sidecar consts.

### Changed

- **Breaking:** `codegen` module renamed to `emit`, and its Cargo features `codegen-batch`/`codegen-row`/`codegen-stream` to `emit-batch`/`emit-row`/`emit-stream` (ADR 0007). In this family *codegen* means what sqlite-rs means by it (AST -> executable VM program, i.e. the planner); the ahead-of-time Rust-source emitter (`render_flat`/`render_joined`/`render_semi_join`/`render_windowed`, `const PROGRAM` in a `.rs` file) is now `emit::batch`. The `codegen` module name is reused for the planner (see below). Consumers of the emitter change `db_core::codegen::batch::*` -> `db_core::emit::batch::*` and the feature name.

- **Breaking:** `sql-types`, `sql-expr`, `sql-parser`, `sql-join`, `sql-vm`, `sql-codegen` merged into one crate, `db-core` (lib name `db_core`), as modules (`types`, `expr`, `parser`, `join`, `vm`, `codegen`). Module boundaries unchanged; only the crate boundary went away. Cargo features renamed to stay unique in a flat namespace: `column`/`row` (parser) -> `parser-column`/`parser-row`; `batch`/`row`/`stream` (vm) -> `vm-batch`/`vm-row`/`vm-stream`; `batch`/`row`/`stream` (codegen) -> `codegen-batch`/`codegen-row`/`codegen-stream`. Consumers depending on the six old crates by name (e.g. `sql-vm = { git = ..., package = "sql-vm", features = ["batch"] }`) need one `db-core` dependency instead, with the renamed features (e.g. `features = ["parser-column", "vm-batch"]`).

### Removed

- `sql-vfs`, `sql-pager`, `sql-header`, `sql-record`, `sql-sys` (#39): moved out of `db-core` into `db-storage`'s new `row` module per [ADR 0006](.openspec/adr/0006-storage-consolidation-into-db-storage.md) -- ADR 0003/0004 updated with pointers to the new location, their extraction reasoning otherwise unchanged. `sql-sys` doesn't move as a crate: `termios` deleted outright (dead code), `fcntl` folded into `db-storage`'s `row::vfs` as a private module (its only consumer, both before and after).

## [0.11.0] - 2026-09-04

### Added

- `sql-codegen`: new crate, structured exactly like `sql-vm` (`batch`/`row`/`stream` modules, matching Cargo feature pattern, `batch` on by default) (#20). `batch` ports column-rs's private `src/codegen.rs` as the canonical rendering layer (`render_flat`/`render_joined`/`render_semi_join`/`render_windowed` and helpers, plus `AggPart`) — generalized to take a `crate_name` parameter instead of hardcoding `"column_rs"` into generated source, so other `sql_vm::batch` consumers can reuse it. `row` is a documented stub recording sqlite-rs's real `src/codegen/*` structure (20,548 lines) as the port target, blocked on `sql_vm::row` (#18) existing first. `stream` is a pure stub, matching `sql_vm::stream`.

## [0.10.0] - 2026-09-03

### Added

- `sql-header`: SQLite database header (bytes 0-99) parsing, extracted verbatim from sqlite-rs's `src/header` (#15) -- pulled in ahead of `sql-pager` since pager's `JournalMode`/`SynchronousMode` enums live here (see ADR 0004 for why). All 16 of its original tests pass unchanged.
- `sql-pager`: page cache, WAL, rollback journal, and freelist management, extracted verbatim from sqlite-rs's `src/pager/*` (#15), built against `sql-vfs` (#14) and `sql-header`. `PagerError`/`WalError`/`JournalError`/`FreelistError` move unchanged in shape, per this session's standing decision not to speculatively centralize error types into `sql-error`. See ADR 0004 for the crate-split adaptations this forced (a `SharedPager` newtype replacing an orphan-rule-violating `impl PageSource for RefCell<Pager>`, a new `sql-vfs` `test-util` feature since `#[cfg(test)]` doesn't cross crate boundaries, and a couple of `pub(crate)` promotions). 78 of pager's tests pass unchanged; its `mod fixtures` integration tests (which need `btree`/`schema`/`record` together) stay in sqlite-rs until those extraction phases land.

### Fixed

- `sql-vfs`: added the missing `[lints.clippy]` baseline (matching sqlite-rs's own and `sql-record`'s/`sql-header`'s) -- its own `#[allow(clippy::unwrap_used, ...)]` annotations assumed this was already enforced.
- `sql-vfs`: `cargo test -p sql-vfs` alone never built its own `src/bin/lock_probe.rs` helper binary (`cargo test` doesn't build sibling `bin` targets automatically) -- the merged `#14` test suite only ever passed locally because `--bins` had been built manually first. `Makefile`'s `test` target now builds `lock_probe` explicitly first, matching sqlite-rs's own Makefile.

## [0.10.0] - 2026-09-03

### Added

- `sql-parser::column`: unary minus/plus (`-x`, `+x`) and `||` string concatenation (#34), growing column-rs's grammar toward DuckDB parity rather than SQLite's — `||` binds looser than `+`/`-`/`*`/`/` (DuckDB/Postgres precedence), deliberately not SQLite's own tighter-binding placement. `sql_expr` gains `BinOp::Concat`/`Expr::Neg`; `sql_vm::batch` gains `MapOp::Concat` (stringifies both operands) and `MapOp::Neg` (`Int`/`Float` negate, `Null` otherwise).

## [0.9.0] - 2026-09-03

### Added

- `sql-parser`: `sql_parser::row::grammar` (sqlite-rs's recursive-descent `Parser`), `row::error` (three-way `ParseOutcome`), and `row::printer` (AST pretty-printer) migrated in unchanged — completes `row`'s parser migration (#23). `row` now re-exports 14 `parse_*` functions and `ParseOutcome` at its module root, mirroring sqlite-rs's own `src/parser.rs`. All 82 of their original tests pass unchanged.
- `sql-vfs`: virtual filesystem abstraction (journal-mode `fcntl` locking, WAL `-shm` reader-mark/checkpoint/write-lock coordination via `pread`/`pwrite`), extracted verbatim from sqlite-rs's `src/vfs/*` (#14). `db-storage`'s separate, minimal, mmap-based `{Vfs, VfsFile}` is deliberately left as its own trait rather than unified with this one -- see ADR 0003 for why (sqlite-rs's own ADR-0001/ADR-0009 already rejected `mmap` for anything with concurrent-mutation exposure, which is `db-storage`'s entire reason to exist for its one read-only consumer). sqlite-rs's own `src/vfs/*` is untouched for now -- switching it over to depend on this crate is tracked separately in sqlite-rs's own repo.

## [0.8.0] - 2026-09-03

### Added

- `sql-parser`: `sql_parser::row::ast`, sqlite-rs's own AST (~15 DDL/DML/transaction/`PRAGMA` statement types) migrated in unchanged — second slice of `row`'s grammar migration (#23). Amends `ADR 0002`: `row` and `column` do not share one AST type after all (folding sqlite-rs's AST into `sql_expr::Query` would redesign an already-tested shape for no consumer that needs the two unified); they still share the Cargo-feature split and `sql_parser::Span`.

## [0.7.0] - 2026-09-03

### Added

- `sql-sys`: vendored POSIX syscall bindings (`fcntl` byte-range locking, `termios` raw mode), extracted verbatim from sqlite-rs's `src/sys/*` -- the lowest-level, dependency-free module in that crate's vendored-syscall layer (#11). db-core's sole `#![allow(unsafe_code)]` carve-out; every other workspace crate `#![forbid(unsafe_code)]`s. sqlite-rs's own `src/sys/*` is untouched for now -- switching it over to depend on this crate is tracked separately in sqlite-rs's own repo.

## [0.6.0] - 2026-09-03

### Added

- `sql-parser`: `sql_parser::row::tokenizer`, migrated unchanged from sqlite-rs's `src/parser/tokenizer.rs` — first real slice of `row`'s grammar migration (#23). Reuses `sql_parser::Span` rather than a second duplicate `Span` type. All 36 of its original tests pass unchanged.

## [0.5.0] - 2026-09-03

### Added

- `sql-parser`: split into `column`/`row` Cargo-feature-gated sections (`column` on by default), mirroring `sql-vm`'s `batch`/`row`/`stream` split (ADR 0001) — decided in `ADR 0002`. `sql_parser::column` holds column-rs's existing grammar (moved unchanged, re-exported at the crate root); `sql_parser::row` is a documented stub reserved for sqlite-rs's grammar migration (tracked separately, #23/#24).

## [0.4.1] - 2026-09-03

### Changed

- `sql-error` folded into `sql-parser` as a module (`sql_parser::span`, re-exported as `sql_parser::Span`); the crate had exactly one consumer, so its own `Cargo.toml`/workspace member was premature modularization (#8). No behavior change.

## [0.4.0] - 2026-09-03

### Added

- `sql-vm`: `Opcode::Window` for window functions (`ROW_NUMBER`, `RANK`, `DENSE_RANK`, `LAG`, `LEAD`, `FIRST_VALUE`, `LAST_VALUE`, `SUM`/`AVG`/`COUNT OVER`), a 1:1 port of column-rs's private `compute_window` — partitions live rows, sorts each partition by `ORDER BY`, and writes one value per row (in original row order) into a `dst` register.

## [0.3.0] - 2026-09-03

### Added

- `sql-vm`: `VmError` now carries `opcode: &'static str` naming the instruction that failed (execution-time error context, matching sqlite-rs's `ExecError` pattern).
- `sql-vm`: `MAX_STEPS` bounded-execution guard (10M instructions), preventing pathological/buggy compiled programs from running indefinitely. `Vm::execute`/`Vm::run` fail with `VmError::StepLimitExceeded` once the limit is exceeded.
- `sql-vm`: `Opcode::name()` method returning each variant's runtime name, used as context in `VmError` messages.

### Changed

- `sql-vm`: `VmError` variants now struct-shaped to carry `opcode` and other context fields, improving error diagnostics.

## [0.2.0] - 2026-09-03

### Added

- `sql-vm`: `Opcode::HashBuild`/`Opcode::HashProbe` for equi-joins, backed by `sql-join::JoinHashTable`. Supports `INNER`/`LEFT`/`SEMI`/`ANTI`; NULL-safe join keys via a new `JoinKey` wrapper. `RIGHT`/`FULL`/`CROSS JOIN` and `Opcode::Window` remain out of scope (tracked separately).
- `sql-vm`: `Vm::clear_registers()`, for callers switching the live register set between a build-side and probe-side program run.

## [0.1.4] - 2026-09-03

### Added

- `sql-error`: `Span` (line/column/byte-offset), threaded through `sql-parser`'s `ParseError` so a consumer (REPL, IDE) can point at *where* parsing failed, not just read a message.
- ADR 0001: layered, synergetic architecture across db-core (`.openspec/adr/`).

## [0.1.3] - 2026-09-02

### Changed

- `sql-vm`: batch/row/stream executors gated behind Cargo features, so a consumer compiles only the one(s) it actually uses.

## [0.1.2] - 2026-09-02

### Added

- `sql-vm`: `BatchExecutor` (implemented); row/stream execution modes stubbed.
- `sql-join`: `JoinKind` and `should_emit` for equi-join semantics (inner/left/right/full/cross).
- `sql-parser`: `SELECT *`, table aliases, `CROSS`/`RIGHT`/`FULL JOIN`, `NOT`, `IS [NOT] NULL`.
- `sql-expr`: `JoinKind::{Right,Full,Cross}`, `SelectItem::Star`, `Expr::{Not,IsNull}`.
- `.openspec/` scaffolding (`adr/`, `specs/`).
- Unit tests for `sql-expr` AST types and `AggFunc::from_name`.

### Fixed

- README: db-core listed only 3 of its 4 workspace crates, omitting `sql-join`.

## [0.1.1] - 2026-09-02

### Added

- `sql-join`: `JoinHashTable`, a flat open-addressing multimap.

## [0.1.0] - 2026-09-02

### Added

- Initial workspace layout: `sql-types`, `sql-expr`, `sql-parser`.
- Makefile (`help`, `build`, `test`, `test-lib`, `lint`, `version`).
