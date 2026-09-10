# ADR 0018: Stream engine — indexed live-log query as storage + prologue/epilogue around the batch VM

## Status

Proposed (#302)

## Decision

The `Stream` execution mode (ADR 0017 `Mode::Stream`) is **not a third
executor**. It is:

1. a storage layer, `storage::stream`, that turns a live append-only file
   into indexed, immutable `Segment`s held in a bounded `Ring`;
2. adapters that present those segments to the batch VM, `vm::batch`
   executor (`impl vm::batch::Segment`, `impl vm::batch::Source`);
3. a planner, `codegen::stream`, that adds a stream **prologue**
   (segment pruning, late parsing) and **epilogue** (windows, standing
   emission) around a body compiled by `codegen::batch`.

Filter, projection, aggregation, sort, limit and the parallel-body →
`Combine` barrier are reused from `vm::batch` verbatim. `vm::stream`
defines only the opcodes the batch VM has no notion of (§Opcodes).

This holds because the batch VM is pull-based (`Source::next_batch()
-> Option<Batch>`), splits a program into a per-segment body and a
finalize barrier (`Program::split_finalize`), resolves columns by name
at run time, and exposes projection pushdown (`Program::columns_to_
load`). A live log is a stream of segments and is consumed through those
four interfaces unchanged.

## Storage: `storage::stream`

```text
file ─────────────────────────────────────────────────────────────▶ EOF
      ▲ tail_off (earliest loaded)             head_off (last indexed) ▲
      │◀──────── Ring: VecDeque<Arc<Segment>> ───────────▶│ head (open)│
   evict whole sealed segments                  append: poll/notify → read from head_off
```

- **`LogFile`** — two cursors, block reads (256 KiB) in both directions.
  Open = seek EOF, read backwards, split on `\n` with partial-line carry:
  tail-first is O(bytes shown), not O(file). Forward fill on demand or on
  idle. Truncation/rotation: `head_off > len` → reset.
- **Two timestamps per line.** `event_ts_ns` (parsed; RFC 3164 has no
  year or zone, so it is not trustworthy alone) and `observed_ts_ns`
  (read time). Event time drives windows; observed time drives "since I
  opened this".
- **`Segment`** — a sealed `LogBatch` (≤ 4096 rows) plus indexes built
  at seal: `minmax` on both timestamps; the Tier-3 dictionaries, which
  *are* the set index (`facility = 'kern'` with no dict hit → skip);
  bloom over `message` tokens and over high-cardinality id columns
  (`trace_id`, `request_id`; default-on, ~1–2 KB); `Vec<u32>` line
  offsets. The head segment is the only mutable object; it is re-indexed
  per refresh and sealed at 4096 rows or an idle timeout.
- **Sparse global index** — `(byte_off, event_ts)` per segment boundary,
  so `since 1h` is a binary search and a seek, not a scan.
- **`Ring`** — new segments enter at the head, whole sealed segments are
  evicted from the tail. The ring is a **cache over the file**: an
  evicted segment is a byte range and is rebuilt through the sparse
  index when a query needs it. Autoscaling: `target = clamp(rate_ewma ×
  default_scope, min, hard_cap)` — the ring's job is to hold at least
  the default query scope.
- **Summaries survive eviction.** When a segment's rows are evicted the
  ring keeps its summary — minmax plus per-aggregate partial states
  (`vm::batch::AggPart`) — for up to the scope horizon. A live
  `count(*) … since 1d` over a 42-minute ring merges a day of summaries,
  re-aggregates the hot ring, and adds the head.

## Indexing: block-level only, no row-level index

The stream engine has **no per-row index** — no B-tree, no hash index,
no inverted index over fields. Inside the ring a filter is a dictionary
lookup plus a vectorized sweep of a `u16` vector over 4096 rows: a few
microseconds per segment, single-digit milliseconds for an hour at
1k lines/s. Below ~10⁷ rows a scan beats any row index, and building one
per field at every seal would cost more than every query saves. Loki's
argument against Lucene-style inverted indexes holds for logs: queries
are time-bounded, so per-row indexes cost far more than they return.

What the engine has instead are **segment-skipping indexes**, each
answering "can this segment contain a match?": minmax (time ranges), the
dictionaries (equality on low-cardinality columns), blooms (needles in
`message` and high-cardinality ids). Hash tables exist only at query
time (`GroupReduce`, `HashBuild`) — execution structures, not indexes.

**Sidecar index file.** In-memory indexes die with eviction and close,
which makes read-through over a large file disk-bound: every segment
would have to be read to consult its bloom. So the skip indexes are also
persisted in an append-only sidecar, `<file>.idx`, one record per sealed
segment: byte range, line count, minmax on both timestamps, blooms.
Read-through prunes from the sidecar and reads only surviving segments;
a second open of a large file is immediate; rebuild is lazy and
incremental from the last indexed byte; truncation/rotation invalidates
it under the same `head_off > len` rule. Size ≈ 2 KB per 4096 lines,
about 0.5 % of the file.

The hot-window "scan is fast enough" claim is **measured, not assumed**:
the adapter benchmark (#302) fixes the numbers before any further index
work.

## Formats

Per-line parsers live in `storage::stream` and fill `LogBatch`. Three
are in scope besides syslog: Apache/nginx combined (raw, positional),
JSON Lines (structured, nested), logfmt (structured, flat). Rules:

- **Tier 2b stays `facility`-only.** Access-log fields (`status`,
  `method`, `bytes`) are typed Tier-3 columns, not new predefined
  columns: a Tier-3 `Int` or `Dict` column filters just as fast and only
  one format has them.
- **Typed Tier-3 at ingest.** `FieldStore` accepts `Int`/`Float`/`Bool`
  as well as `Str`; first non-null type wins, conflict degrades to `Str`;
  sparse keys are null-padded.
- **Never fabricate Tier-2.** A format without a severity (combined
  log) leaves `severity = None`; the query says `status >= 500`.
- **Alias promotion** for structured formats (`time|ts|@timestamp…` →
  `event_ts`, `level|severity|lvl…` → `severity`, `msg|message…` →
  `message`); numeric levels (Bunyan/Pino 10–60) map to `Severity`.
  Nested objects flatten to dot paths to depth 2; deeper stays a string
  for late `json_extract`.
- **Two-stage container parse.** Docker `json-file` and Kubernetes CRI
  wrap a payload in another format; the container is parsed first, the
  payload re-enters detection. Container fields stay Tier-3.
- **Detection locks on per file** (sample the first block; ≥ 80 %
  agreement decides), falling back to per-line detection only when
  lock-on fails; `format` is recorded as a Tier-3 `Dict` column.

## Scope and retention

Every stream query has a **scope** — how far back from EOF it covers.
Scope is per query; **retention** (what the ring holds hot) is the
ring's business. They are deliberately separate: a wider scope costs
disk reads, a bigger ring costs memory.

```rust
enum Scope { Time(Duration), Lines(u64), Bytes(u64), All }
```

Resolution, most specific wins: in the query (`since 1h`, `since 100000
lines`, `since 256mb`) → CLI (`--scope`) → config (`default_scope`) →
built-in `1h`. All three units lower to one byte range through the
sparse index. `ORDER BY ts DESC LIMIT n` means `since n lines`.

**Default scope is what makes every operator well-defined.** `ORDER BY`,
`SUM`, `JOIN` each need to see the last row before producing the first;
on an unbounded stream that row never arrives. With a scope, each has a
boundary and compiles to an ordinary batch query. In follow mode an
aggregate becomes a sliding window of `scope`, re-emitted on change.

**Every result reports its effective range** — lines, first/last
timestamp, scope requested, scope available, cap. A default that is
silent is a wrong answer waiting to be trusted.

Scope wider than the ring **reads through**: segments beyond `tail_off`
are built transiently, streamed through the VM one at a time, and
dropped; the ring does not grow. Past the file start the range is
clamped and reported.

## Planner: `codegen::stream`

`compile(select: &Select, scope: Scope) -> Result<Program>`:

1. **Validate by rejecting** (ADR 0002): one grammar, one AST; the
   stream subset is enforced after a full parse with a `Span`-carrying
   error, never by parsing less. v1 rejects joins and any blocking
   operator whose only boundary is `Scope::All` in follow mode. Rejection
   is at planning time — before a byte of the log is read — so the
   message can name the aggregate and the boundary that would fix it.
2. Lift `since`/`until`/`between` (or the default scope) and the
   index-pushable predicates (time ranges, `=` on dictionary columns) into
   `Prune`; keep the residual.
3. Delegate the residual `WHERE`, projection and aggregates to
   `codegen::batch::compile` on the rewritten `Select`.
4. Wrap: prologue, `Body`, epilogue.

`Scope` is a planner input; its default lives in the client, not in
db-core.

Exact decomposition holds for `COUNT`/`SUM`/`MIN`/`MAX`/`AVG`.
`COUNT(DISTINCT)` and quantiles do not decompose across summaries; they
fall back to read-through (reported) until sketch states exist.

## Opcodes: `vm::stream`

Typed operands, ADR 0007 style. Only what `vm::batch` cannot express.

| Opcode | Meaning | Reference |
|---|---|---|
| `Prune { scope, preds: Vec<IndexPred> }` | segment selection via minmax / dictionary / bloom before any row is materialized | ClickHouse skip indexes (`minmax`, `set`, `bloom_filter`); Loki chunk blooms |
| `Parse { src, format: Json \| Logfmt \| Regex \| Kv, prefix }` | late structuring: new Tier-3 columns for this query only, after the cheap substring filter on `raw` | Loki `\| json` / `\| logfmt` pipeline stages; ClickHouse raw `Body` + materialized columns |
| `Body(Vec<vm::batch::Opcode>)` | filter / project / reduce — reused verbatim | ADR 0007 `split_finalize` |
| `Window { size, step, key }` + `RangeAgg { func }` | `count_over_time`, `rate`, `*_over_time` over `[5m]`; tumbling/hopping keyed by dictionary index | Loki range vectors; Kafka Streams windows |
| `Watermark { grace }` | close windows at `max(event_ts) − grace`; late lines to a side output | Flink event time / watermarks |
| `Emit { mode: Rows \| OnChange \| Threshold(expr) }` | standing-query output; alerts are `OnChange`/`Threshold` on a result set | Loki ruler (`for`), swatchdog |

Storage mechanisms and their references:

| Mechanism | Reference |
|---|---|
| Incremental line-offset index rebuilt from the last indexed byte; truncation reset; SQL virtual tables over a growing file | lnav `logfile::rebuild_index` |
| Immutable segment per refresh; queries union segments; refresh interval bounds re-index cost | OpenSearch/Lucene segments + `refresh` |
| Labels indexed, line content not; cheap line filter before parse | Loki |
| Per-block minmax / set / bloom skip indexes; `ORDER BY (low-card, ts)`; incremental aggregation via partial states | ClickHouse MergeTree / `AggregatingMergeTree` |
| Pull-based `RecordBatch` iteration with constant memory; blocking operators only over bounded input | DuckDB / Arrow streams |
| `Timestamp` vs `ObservedTimestamp` | OpenTelemetry log data model |
| Per-row transforms and windowed aggregates as distinct operators | Vector VRL + `aggregate`/`reduce` |

## Structure

- `src/storage/stream/{file,segment,ring,index,sidecar}.rs` — `LogFile`,
  `Segment`, `Ring`, sparse index, `<file>.idx` sidecar.
  `batch.rs` and the parsers `syslog.rs`, `clf.rs`, `jsonl.rs`,
  `logfmt.rs` plus `detect.rs` (lock-on) are the per-line parse into
  `LogBatch`.
- `src/storage/stream/adapter.rs` — `impl vm::batch::Segment for
  Arc<Segment>` (materializes only `Program::columns_to_load()`),
  `TailSource: vm::batch::Source` (blocks on head refresh).
- `src/vm/stream.rs` — `Opcode`, `Program`, the prologue/epilogue
  executor that drives `vm::batch::Vm::execute` for `Body`.
- `src/codegen/stream.rs` — `compile`, `Scope`, `IndexPred`, validator.
- `src/engine/stream.rs` — `StreamEngine: Engine` (ADR 0017);
  `FileStats::Stream { bytes_parsed, lines }` plus ring/scope stats.
- Features: `storage-stream = ["vm-stream"]`, `codegen-stream
  = ["codegen-batch", "storage-stream"]`, `engine-stream = [...]`.

## Consequences

- Dictionary → `vm::batch::Value::Str` materialization per row is the
  performance frontier. It is measured before it is optimized; the fix,
  if one is needed, is a dictionary column variant in `Batch`, not a
  stream-specific value model (ADR 0010, ADR 0014).
- The head segment re-materializes on every refresh; refresh rate is
  capped (OpenSearch's 1 s default exists for this reason).
- Alerts need no server: a standing query is a compiled program plus an
  interval and a `for` duration, evaluated on each segment seal in the
  client process.
- The stream mode consumes the full parser through `codegen::stream`,
  as the row and batch modes do through their planners; there is no
  expression-only parser entry point.
