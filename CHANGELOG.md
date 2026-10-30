# Changelog

All notable changes to db-core. Format follows [Keep a Changelog](https://keepachangelog.com/), versioning follows [SemVer](https://semver.org/). Pre-1.0: minor bumps may break the public API.

**Versioning policy:** one crate, one version, one tag per release.

## [0.203.0] - 2026-10-30

### Changed

- **A join on a unique, dense integer key looks up its build row by position** (tools#88). When every build key is an `Int`, no key repeats and the keys span at most 4 slots per build row, `HashBuild` also keeps a direct index (`key - min` → build row), and the three join probes (`HashProbe`, `HashProbeGroupReduce`'s grouped and general folds) find a probe row's match with one array read. They skip the per-row hash and the slot walk through the 70%-load hash table. Any other key keeps the hash table, and results are unchanged: only an `Int` probe key matches, and NULL never does. On column-rs's parity join (10M fact rows joined to 100,000 customers keyed `0..99,999`, hyperfine mean of 30): 79.1 → 56.2 ms wall, CPU 558 → 307 ms, 1.66x → 1.18x DuckDB. Peak RSS barely changes (274 → 263 MB).

## [0.202.0] - 2026-10-29

### Added

- The Parquet footer reads each column chunk's `encodings` (`ColumnMetaData.encodings`) and the `LogicalType` union (`SchemaElement.logical_type`: `STRING`, `LIST`, `MAP`, `DECIMAL`, `TIMESTAMP` with its unit and `isAdjustedToUTC`, `INTEGER` and the rest), for tools that describe a file (tools#86). `Value::as_bool` for the Thrift decoder.
- `Manifest::version()`: the format version a manifest is written as (tools#86).

## [0.201.0] - 2026-10-29

### Added

- List transforms (#828): `array_to_string`, `list_aggr` / `list_aggregate` (`string_agg`, `count`, `min`, `max`, `first`, `last`), `list_distinct`, `list_unique`, `list_sort`, `list_reverse_sort`, `list_reverse`, `list_slice` and `list_concat` (`list_cat`, `array_concat`, `array_cat`), with their `array_` aliases and DuckDB's NULL semantics. An unknown `list_aggr` aggregate or `list_sort` order is refused at plan time.

## [0.200.0] - 2026-10-29

### Added

- List literals `['a', 'b']` in the column dialect, with `list_has_any` / `list_has_all` and `list_position` (and their `array_` and `list_indexof` aliases), with DuckDB's NULL semantics (#827). A list of non-string literals such as `[1, 2]` is refused.
- Fixed a call with no arguments, such as `list_value()`, returning the batch's row count instead of the rows left after a `WHERE` (#827).

## [0.199.1] - 2026-10-29

### Fixed

- `l[i]` on a `LIST<VARCHAR>` returned NULL; it now reads element `i` (1-based, negative from the end), as do `list_extract` and `list_element` (#826). `len(l)` and `array_length(l)` count a list's elements. A scalar function's arguments in the SELECT list may be any expression (`tags[-1]`, `len(string_split(s, ','))`).

## [0.199.0] - 2026-10-28

### Added

- `list_contains(list, value)` and its aliases `list_has`, `array_contains`, `array_has` (#825): test a `LIST<VARCHAR>` column or `string_split(...)` for a value, in `SELECT` and `WHERE`, with DuckDB's NULL semantics.

## [0.198.0] - 2026-10-28

### Changed

- Faster `COUNT(DISTINCT <string>)` with many small groups: no per-group allocation for a group with one string (#823).

## [0.197.0] - 2026-10-28

### Changed

- Faster `COUNT(DISTINCT x)` over a string column, grouped or not, including after `UNNEST`: 2.8x on a high-cardinality column, within 3x of DuckDB at 300k events (#820).

## [0.196.1] - 2026-10-28

### Fixed

- Fixed a slowdown in `UNNEST` over a `LIST<UTF8>` column with very many distinct values (about 1M): such a column is now read as plain strings, as before 0.195.0, instead of through a hashed dictionary (#809).

## [0.196.0] - 2026-10-28

### Changed

- Faster `COUNT(DISTINCT x)` over an integer column, grouped or not, including after `UNNEST` (#817).

## [0.195.0] - 2026-10-28

### Changed

- Read a `LIST<UTF8>` column that only `UNNEST` reads as a flat offsets + dictionary layout: `UNNEST ... GROUP BY` is about 2.3x faster and uses about 55% less memory on a 10M-row file (#809).
- Levels-only list reads are no longer disabled by an `ORDER BY` on the list's query (#810).

## [0.194.0] - 2026-10-28

### Changed

- Faster `UNNEST(string_split(...))`, grouped `count(DISTINCT <string>)` after UNNEST, and `=`/`<>` on string columns (#815, partial).

## [0.193.0] - 2026-10-28

### Changed

- Read a `LIST<UTF8>` column for `IS [NOT] NULL` without decoding its elements: about 4-5x faster and about a third of the memory for that query at 10M rows (#810).

## [0.192.0] - 2026-10-28

### Added

- More than one `count(DISTINCT ...)` in a query, with or without `GROUP BY` and beside other aggregates, as DuckDB answers it: `SELECT label, count(DISTINCT event_id), count(DISTINCT source) FROM events, UNNEST(labels) AS l(label) GROUP BY label` (#807).

## [0.191.0] - 2026-10-28

### Added

- `string_split(s, sep)` (alias `str_split`) in the column dialect: a `LIST<VARCHAR>` as DuckDB gives it (`''` is `['']`, NULL is NULL, a NULL separator keeps the string whole, an empty one splits into characters), so a delimited string can be unnested: `FROM events, UNNEST(string_split(labels, ',')) AS l(label)` (#803).

## [0.190.0] - 2026-10-28

### Added

- `UNNEST` of a `LIST<VARCHAR>` column in the column dialect, in `FROM` (`FROM events, UNNEST(labels) AS l(label)`, `CROSS JOIN UNNEST(...)`) and in the `SELECT` list: one row per element, none for a NULL or empty list, a NULL row for a NULL element. It runs over Parquet files and table directories (files and WAL pages), and `EXPLAIN` shows the step (#802).

## [0.189.0] - 2026-10-28

### Added

- LIST<VARCHAR> columns: record tables take a list per row, written to Parquet as DuckDB and pyarrow read `VARCHAR[]`, and `SELECT` returns the list as a JSON array (#801).

## [0.188.0] - 2026-10-26

### Added

- A Parquet/SQLite star join may name table directories as well as Parquet files: the largest table the query is a star around probes, the others build in memory, and rows in a directory's live pages take part (#782).

## [0.187.0] - 2026-10-26

### Added

- `CrossModeEngine::open_table_sqlite`: a table directory (archive files and live pages) joins SQLite lookup tables, inner, left and star, with `EXPLAIN` and the lookup-filter row-group pruning of the Parquet case (#772). Adds a `CrossModeEngine::TableSqlite` variant.
- `snapshot_segments_where` also skips the row groups of a file whose footer statistics prove the filter matches none of its rows (#772).

## [0.186.0] - 2026-10-26

### Added

- `CASE` (searched and simple), `COALESCE`, `IFNULL` and `NULLIF` in the column dialect and the stream engine, with DuckDB's answers and result types: a DECIMAL stays a DECIMAL, an integer next to a float or decimal is promoted, text next to a number is an error. A branch is computed only for the rows that take it, so an overflow in a branch no row takes is not an error, and a column has one type whatever the segments (ADR 0030).
- `TRUE` and `FALSE` literals are booleans in the column engine; they compiled to NULL.

## [0.185.0] - 2026-10-26

### Added

- A Parquet/SQLite star join may name a second Parquet table: the larger one probes, the smaller builds in memory, and a filter on the built table prunes the probe's row groups. `EXPLAIN` covers star joins and shows which side probes (#782).

## [0.184.0] - 2026-10-26

### Added

- Column dialect and batch VM: `ROUND(x)`, `ROUND(x, n)` and `ABS(x)`, including over aggregates such as `ROUND(SUM(x) / 100.0, 2)`; they match sqlite3 on half-way cases (2.5, −2.5) and NULL (#788).

## [0.183.0] - 2026-10-26

### Changed

- `TableReader` keeps its parsed records across a flush (it drops what the flush took into files and parses only what comes after) and keeps a lines table's schema, so a reader in another process no longer re-parses the tail each hour or reopens every file footer per query.

## [0.182.0] - 2026-10-26

### Added

- `snapshot_segments_with_bit_patterns`, `file_segments_with_bit_patterns` (now public), `read_snapshot_with_bit_patterns` and `decode_column_full_bit_pattern`: a caller names its join-key columns, and a `UINT_64` above `i64::MAX` in them reads as its signed bit pattern in flushed row groups and record pages, on a join's probe and build sides. Elsewhere it still errors (#786).

## [0.181.1] - 2026-10-26

### Fixed

- `EXPLAIN QUERY PLAN` for a cross-mode join reports a SQLite lookup table's real row count (from `ANALYZE` statistics, or `COUNT(*)`) instead of `1 row group, ~0 rows` (#789).

## [0.181.0] - 2026-10-26

### Added

- `approx_count_distinct(x)` (#603): HyperLogLog estimate of distinct values in the batch engine, grouped or not; about 0.8 % standard error at precision 14, deterministic across segment splits.

## [0.180.0] - 2026-10-26

### Added

- `TableCursor::seek_ge` and `TableCursor::scan_u64_range`: lookups and range scans over row keys at or above 2^63 (stored as signed bit patterns) in unsigned order.
- A `UINT_64` Parquet join key above `i64::MAX` reads as its signed bit pattern, so it joins to the matching SQLite row key (#600). Other reads above `i64::MAX` still error.

## [0.179.0] - 2026-10-26

### Added

- `TableReader`: an incremental reader for a table written by another process. A refresh parses only the WAL records added since the last one, and `TableEngine` uses it, so repeated queries no longer re-parse the whole tail. A fresh read of a 1M-record tail takes 243 ms against 0.7 ms for a refresh.

## [0.178.0] - 2026-10-26

### Changed

- Join name resolution moved into the planner (`codegen::batch::resolve_join_columns`), so any join caller gets the same ambiguous-column and `*`/`t.*` rules (#778).

## [0.177.0] - 2026-10-26

### Added

- Exact per-file key ranges for columns outside the sort key (`TierOptions.stat_columns`), pruned in every snapshot reader.
- Filtered snapshot readers for join build sides and window input (`SnapshotFilter`, `read_snapshot_where`, `snapshot_files_where`, `snapshot_segments_where`).
- `query_snapshot_files` reports how many files a query opens.

## [0.176.0] - 2026-10-25

### Added

- **`JOIN ... USING (k)`** (#771): the `FROM` table's value for an unqualified `k`. `USING (a, b)` hashes on `a` and filters on `b`, so it is inner-only; a `LEFT JOIN` with several columns is an `Unsupported` error. Works for Parquet/SQLite and stream/SQLite joins and prunes row groups from lookup filters like `ON`.
- **`SELECT *` over a cross-mode join** (#771) lists every table's columns in the order written, the `USING` key once; it used to fail with "emit has no registers".
- **`ON j.k = f.k` with the joined table's column first** (#771) joins instead of failing with "unknown column".
- **Unqualified columns in a cross-mode join resolve like SQLite and DuckDB** (#771): the one table that has the name, an `ambiguous column name` error when several do (except a `USING` key), where a bare name used to mean the `FROM` table silently. `SELECT *` headers are bare names.
- **`t.*` in a select list** (#771): one table's columns by name or alias in a cross-mode join, the whole row in a single-table query; it was refused. Expression headers of a cross-mode join stay as written (`SUM(amount)`).

## [0.175.0] - 2026-10-25

### Added

- **Session-key time pruning** (#599). `TierOptions.session_key: Option<SessionKey>` names a key column (`Int64` or `UInt64`) whose high 32 bits are the session's start in seconds, and the longest a session runs (`max_session_secs`). A session's rows arrive no earlier than its start and no later than `start + max_session_secs`, so `query_snapshot` turns `=`, `IN (...)` and range predicates on the key (with `AND`/`OR`) into the arrival window each file can hold and skips files whose `observed_ts` lies outside it, before opening any footer. It reuses the #589 pruner: a file's arrival range `[lo, hi]` bounds the keys it can hold to `[(lo_s - max) << 32, (hi_s << 32) | 0xFFFF_FFFF]`, saturating. The open hour and WAL pages are never pruned, a file without arrival bounds is kept, and any predicate shape it cannot bound (`<>`, `OR` with a non-key arm, functions) keeps every file. The option is recorded in the manifest (version 6, with `evolved` named explicitly), since readers in other processes prune with it, so an older binary refuses the table. Without the option, results and files read are unchanged. The bound is a contract: a row arriving later than `start + max_session_secs` is hidden from a query that filters on its key.

## [0.174.0] - 2026-10-25

### Added

- **A Parquet table joins SQLite lookup tables** (#598). `CrossModeEngine::open_column_sqlite(parquet, sqlite)` and `resolve::run_column_query` run `events JOIN event_def ON events.event_id = event_def.event_id` with the Parquet file (named after its stem) as the probe side, one segment per row group, and the SQLite tables as the `HashBuild` side, one table or a star of several. Same rules as the stream/SQLite join: the lookup side builds wherever the query writes it, a plain `JOIN` is inner with no semantic change, `LEFT JOIN` needs the Parquet table first, and `lookup LEFT JOIN parquet` (which would keep the lookup rows) is `ErrorKind::Unsupported` with the way to write it. `EXPLAIN QUERY PLAN` labels the Parquet and SQLite sides by mode and file and adds a `PRUNE ROW GROUPS` row (`N of M kept`) when a lookup filter prunes; `EXPLAIN` lists the SQLite build, Parquet probe and body opcodes (#770). `EXPLAIN` of a star join is `Unsupported`, as for stream (#394).
- **A filter on a lookup table prunes the Parquet row groups** (#598). For each `INNER JOIN`, the `WHERE` conjuncts that name only that lookup table's columns (`WHERE event_def.name = 'x'`) run over the materialized lookup table to get the matching join keys; the probe file then prunes with `event_id = k1 OR event_id = k2 ...` (past 256 integer keys, the range `[min, max]`) through the footer statistics, and no matching key at all skips the probe. The query's own `WHERE` still runs after the join, so the predicate only drops row groups that cannot contribute. A `LEFT JOIN`'s table, unqualified columns, an `OR` across tables and function calls are not used. No new opcodes; single-table queries are untouched, and `resolve_sides`/`resolve_multi_sides` now take the driving table's name instead of assuming `log`.
- **`make check-column-join-oracle`** (#598): 27 Parquet/SQLite join queries (inner, left, swapped order, lookup filters, text and integer keys, star joins) compared with DuckDB reading the Parquet file and attaching the SQLite file, via `examples/column_join_oracle`. Local-only, like `check-column-oracle`.

## [0.173.1] - 2026-10-24

### Changed

- **`RunningAgg` keeps `MIN`/`MAX` as primitives for integer and float input** (#763). `push_i64`/`push_f64` boxed a `Value` and ran `keep_extreme` -> `agg_cmp` -> `numeric_cmp` twice per row for every aggregate, `COUNT` and `SUM` included (about half the CPU of `count(amount)`). The extremes are now an `Extremes` enum: a plain `i64` / `f64` (`total_cmp`) pair while the input is all integers or all floats, boxed values from the first input of another type on, with the primitives carried over, so results are those of the old sequential fold, first-seen ties (`MIN(1, 1.0)`) and precision-loss ties past 2^53 included. The accumulator stays the same size. A/B/A against main through column-rs, wall / CPU ms: 20M rows with nullable INT64 dictionary `amount`, `count(amount)` 33 / 267 -> 25 / 172, `sum(amount)` 33 / 267 -> 25 / 171, `GROUP BY kind` with `count(*), sum(amount)` 49 / 428 -> 37 / 320; parity `large` (DOUBLE `amount`), `SUM` 23 / 145 -> 15 / 97, `GROUP BY region` 230 -> 169 ms CPU, `GROUP BY customer_id` (high cardinality) unchanged at 130 / 950, same peak RSS.

## [0.173.0] - 2026-10-24

### Added

- **`ORDER BY <ordinal>` in the column engine** (#752). An integer term `k` names the `k`-th select item, 1-based as in SQLite and DuckDB: its alias when it has one, otherwise its column or aggregate expression, so `GROUP BY user_name ORDER BY 2 DESC, 1` sorts on `count(*)` then the key. `ORDER BY 0` or past the select list is sqlite3's `1st ORDER BY term out of range - should be between 1 and N`; an ordinal naming `*` or an unaliased computed expression is refused with a hint to alias it. New sqllogictest file `order_by_ordinal_752.test` against DuckDB.
- **`ParseError::Unsupported`** (#752): the column validator's rejections of SQL that parsed fine now read `unsupported at L:C: ...` instead of `unexpected token at L:C: ...`. Grammar failures keep `ParseError::Unexpected`. An exhaustive `match` on `ParseError` needs the new arm.

### Changed

- **Dictionary-encoded INT32/INT64/DOUBLE/FLOAT chunks decode straight into typed columns** (#753). `RowGroupReader::read_dictionary_fixed_column` decodes the dictionary page once through the caller's decode (INT32 and FLOAT widen per entry, not per row) and gathers each `PLAIN_DICTIONARY`/`RLE_DICTIONARY` page's codes into `(values, present)`, with a `PLAIN` fallback page read in place and a bad code as `DictionaryIndexOutOfRange`; `decode_column_full` tries it after the `PLAIN` reader, before the `Vec<Option<T>>` readers. The positional `decode_column_at` is unchanged. A/B/A against main through `BatchEngine` (20M rows, DuckDB-written, uncompressed, `amount` PLAIN_DICTIONARY, best of 7): `count(amount)` 25.2 -> 22.5 ms wall, `sum(amount)` 26.6 -> 22.7 ms, CPU 262 -> 236 ms a run, peak RSS 108-116 -> 84-92 MB; a dictionary chunk now costs what the PLAIN `id` chunk does (233 ms CPU).

## [0.172.1] - 2026-10-24

### Changed

- **A high-cardinality `GROUP BY` on a dictionary column folds a segment's slices by code** (#751). Since #643 a segment runs in `SLICE_ROWS` slices, and each slice's `GroupReduce` emitted its own partial groups with a freshly allocated key string, so `Combine` merged about eight times segments × groups partial rows. `execute_in_slices` now carries the groups of a single-key `GroupReduce` → `Emit` body across the slices of a segment as dictionary codes, with one set of accumulators, and the segment emits one partial row per group in first-seen order. A slice whose key is not codes under the same dictionary (a short last slice decodes a large dictionary to `Str`) hands on the fold first and runs as before, so results and group order are unchanged. `Combine`'s generic key index slots by the Murmur `SlotState` (#514) instead of SipHash. On column-rs (20M rows, 163 row groups, hyperfine mean of 20, A/B against main): `SELECT user_name, count(*) … GROUP BY user_name ORDER BY n DESC, user_name LIMIT 5` (10,007 groups) 134 → 54 ms wall, CPU 1352 → 457 ms (2.9× DuckDB's 158 ms, was 8.6×); `GROUP BY kind` with `sum(amount)` unchanged at 44 ms; peak RSS unchanged. New bench `engine::run Dict GroupReduce+Combine` 109 → 26 ms.

## [0.172.0] - 2026-10-24

### Changed

- **`engine::column::file_segments` takes the query's `WHERE` clause and prunes row groups by footer statistics** (#747, breaking). `file_segments(file, columns, where_clause, program)` passes the clause to the same pruning `BatchEngine` does (#458): a row group whose min/max (or dictionary, for `=`) proves no row can match is left out. column-rs, which loads Parquet files through `file_segments`, never pruned; it passes `select.where_clause.as_ref()` (`None` keeps every group, as before). Pruning only drops groups the filter would empty anyway, so results are unchanged; a test pins that against the unpruned run. Measured with column-rs patched to pass the clause (20M rows, 163 row groups, ascending `id`, hyperfine mean of 10): `count(*) WHERE id < 100000` 51 -> 6 ms wall (CPU 428 -> 4 ms) and `WHERE id > 19900000` 50 -> 7 ms, both reading 1 of 163 row groups, with the same counts. column-rs needs a matching change (`Opened::segments` passing `select.where_clause`), a separate tools PR.

## [0.171.2] - 2026-10-24

### Changed

- **`=`, `<>`, `<`, `<=`, `>`, `>=`, `AND` and `OR` over typed `Int`/`Float`/`Bool` columns run on the buffers, not one `Value` a row** (#748). `Vm::typed_compare` takes an `Int`/`Float` column against a numeric literal (either side) or another column and yields a typed `Bool` column, as `typed_like` does (#743); `Vm::typed_bool_op` combines two typed `Bool` columns with SQL's three-valued logic (`three_valued`'s answers, now shared through `kleene`). So `x BETWEEN a AND b` is three typed passes that `Filter` reads without materializing. The answers are `apply_map_op`'s: both sides compare as `f64` as `compare_values` does (an `Int` past 2^53 rounds, a NaN is unequal to everything), and a NULL operand is a NULL row. A string, NULL or decimal literal, or two literals, still take the general path. Measured end to end on column-rs (20M rows, 163 row groups, hyperfine mean of 10, A/B/A): `count(*) WHERE amount >= 100` 60 -> 28 ms wall (CPU 430 -> 173 ms); `WHERE amount BETWEEN 100 AND 200` 113 -> 32 ms (CPU 874 -> 232 ms); `WHERE id BETWEEN 100 AND 2000000` 121 -> 37 ms (CPU 927 -> 271 ms), peak RSS 167 -> 125 MB; the `count(amount)` control is unchanged at 41 ms.

## [0.171.1] - 2026-10-24

### Changed

- **`col IN (literal, ...)` is one set probe per row, not a comparison and an `OR` per item** (#737). The column parser still lowers an `IN` list to `col = a OR col = b ...` (`col <> a AND ...` for `NOT IN`), so validation, pruning and the stream engine are unchanged; batch codegen recognizes that chain over one column and literals, also written out by hand, and emits a new `Opcode::InList`. A typed string or dictionary column probes a hash set of the list's strings in the column's own buffer (a dictionary once per distinct value) and yields a typed `Bool` column; any other column or list folds the same comparisons in one pass, so conversions, NULLs and errors match the chain exactly. On a 2M-row tiered table (UUID strings, ZSTD, best of 5, A/B/A against main): `IN (100 ids)` 1 140 ms → 21 ms, the same behind `ts >= now - 2 days` 80 ms → 4 ms, `NOT IN (100)` 1 150 ms → 44 ms; a single `=` is unchanged at 30 ms. File pruning already handled the lowered chain; a test now pins it for `IN`.

## [0.171.0] - 2026-10-24

### Added

- **A record table's schema can gain nullable columns at the end** (#740). `TableWriter::open` accepts a requested schema that is the stored one plus nullable columns and commits it to the manifest before anything is appended; a dropped, renamed, retyped, reordered or non-nullable column still fails with `SchemaMismatch`. Rows that predate the change read NULL in the new columns: WAL rows of the old arity are padded on replay (writer and `Snapshot`), and a day merge reads a nullable column an older hour file lacks as NULL instead of `Corrupt("has no column ...")`. A manifest whose schema evolved is written as version 5 (`Manifest::evolved`), so a reader that predates it refuses the table rather than reading files and rows that lack the columns.

## [0.170.1] - 2026-10-24

### Changed

- **`count`/`min`/`max` over a `Str` or `Dict` column reduce on the typed column, not one `Value` a row** (#750). `typed_reduce_values` now counts from the validity bitmap and orders strings in place, byte-wise as `agg_cmp` does; a dictionary column marks the codes in use and orders only those entries. `sum`/`avg` stay NULL, as before. Measured end to end on column-rs (20M rows, 163 row groups, hyperfine mean of 10, A/B/A): `count(kind)` 86 -> 14 ms wall (CPU 862 -> 81 ms); `min(user_name), max(user_name), count(note)` 222 -> 81 ms (CPU 2325 -> 779 ms); the `count(amount)` control is unchanged at 32 ms. Results equal `reduce_values`' for NULLs, empty strings, all-NULL and zero-length columns, and unused dictionary entries.

## [0.170.0] - 2026-10-24

### Fixed

- **A nanosecond TIMESTAMP compares with a quoted time in a single-file query** (#679 follow-up). `BatchEngine` found TIMESTAMP columns through `ConvertedType` only, which has no nanosecond value, so `ts >= '2026-10-22T01:00:00Z'` over a TIMESTAMP(NANOS) column (what a db-core table writes) failed with "could not convert string ... to INTEGER". The footer now reads the `LogicalType` timestamp unit (`SchemaElement::timestamp_units`).
- **A quoted time is parsed without reading the clock** unless it is `'now'`. The TIMESTAMP literal rewrite called `unixepoch` through the clock-reading path, which panics on `wasm32-unknown-unknown` (no `SystemTime`).
- **Batch queries run on a host without threads.** The morsel pools (`run_morsels`, `run_morsels_fold`, `run_morsels_ordered`) run on the calling thread when they would get one worker, instead of spawning one. `wasm32-unknown-unknown` reports no parallelism and cannot spawn, so a query there panicked (`unwrap_failed` in `Scope::spawn`); a one-core host also skips the spawn.

## [0.169.4] - 2026-10-24

### Changed

- **`LIKE`/`GLOB` over a typed string column yields a typed `Bool` column, not one `Value` a row** (#743). The result of `typed_like` is a `Column::Bool` register, one byte a row plus the validity bitmap, which `Filter` reads directly; anything else converts it lazily, as it does for `LoadColumn`. NULL rows stay NULL. Measured end to end on column-rs (20M rows, ZSTD) the change is within noise on wall time, CPU and peak RSS; the per-row `Value` was about 2% of the CPU, and the rest is ZSTD decode and the matcher.

## [0.169.3] - 2026-10-24

### Changed

- **`timediff` drops a redundant loop guard and keeps the other as a plain check** (#744). The `<` loop tested `d1.ijd < d2.ijd && !d2.is_error`, but an errored date has `ijd == 0`, so the second test could never change the result; the loop is now `while d1.ijd < d2.ijd`, as in sqlite3. The `>` loop keeps its stop against an endless loop on an errored `d2` as an `if d2.is_error { break; }` in the body. Results are unchanged; the `MC/DC` dashboard has no outstanding obligations left in `src/functions/datetime.rs`.

## [0.169.2] - 2026-10-24

### Changed

- **`LIKE`/`ILIKE`/`GLOB` build each literal segment's search table once per pattern, not once per row** (#732). `str::find` rebuilt its searcher on every call, which a profile of `WHERE note LIKE '%host 42%'` over 20M rows put at about 17% of all CPU. Each segment now keeps a linear-time search table built in `Matcher::new`, so an adversarial pattern such as `a…ab` against `aaaa…` stays linear. Results are unchanged.

## [0.169.1] - 2026-10-24

### Fixed

- **`engine-column` with `ingest-read` did not compile** (#738). The decoders `engine::column::table` uses to read a stored string column (`stored_column_values` and the JSON, logfmt and CLF helpers behind it) were gated on `ingest` alone, so the feature set column-rs enables failed with an unresolved import. They now build under `ingest`, or under `ingest-read` with `engine-column`. `make check-features` also builds that combination and column-rs's full feature set, so a helper gated on one feature but used under another can no longer pass the per-feature check.

## [0.169.0] - 2026-10-23

### Changed

- **The ZSTD decoder is about 30% faster** (#733). The sequence loop reads bits from a 64-bit container reloaded once per sequence instead of a checked load per read, each sequence table entry carries its value baseline and extra-bit count so no code-table lookup follows, the first 16 bytes of a literal run or match are copied unconditionally with longer ones out of line, and the four Huffman literal streams decode in alternating groups of five symbols so their dependency chains overlap. On a 1 MB text page (level 3) 905 -> 1155 MB/s against libzstd's 1920; on an 8 MB log file 800 -> 1060 MB/s. No `unsafe`, no new dependencies.

## [0.168.1] - 2026-10-23

### Changed

- **A `PLAIN` string column chunk decodes straight into `Column::Str`** (#731). A chunk with no dictionary (or one an encoder wrote as `PLAIN`, such as a high-cardinality `note`) used to allocate one `String` per row and then copy it into the column's buffer. The values now go into one buffer as they are read, and UTF-8 is validated once over the whole buffer plus a check that no value starts mid-character, which together make each value valid on its own. A value that splits a multi-byte character is still `InvalidUtf8`, a truncated prefix or value `UnexpectedEof`. A chunk with a dictionary page, or any other encoding, takes the earlier path. The positional reader is unchanged.

## [0.168.0] - 2026-10-23

### Added

- **`INSERT`, `UPDATE` and `DELETE` on `WITHOUT ROWID` tables that have no secondary index** (#722, part of #696). A table made by sqlite3 can now be written, not only read: rows go into its index b-tree in key order, the `PRIMARY KEY` columns are `NOT NULL`, a repeated key fails with `UNIQUE constraint failed: t.a, t.b` (and `INSERT OR IGNORE`/`OR REPLACE` behave as in sqlite3), a key is probed with the column's affinity, and an `UPDATE` that changes a key deletes and re-inserts the row. The differential tests check both files with `PRAGMA integrity_check` and read each one back with the other engine. A table with a secondary index, an upsert, `INSERT ... SELECT` from the table being written, a `DESC` or non-`BINARY` key, and `CREATE TABLE ... WITHOUT ROWID` are still refused, each with its own message; they land in the next pull requests.

### Fixed

- **A statement that failed outside a transaction kept the rows it had already written.** `INSERT INTO t VALUES (1, 'a'), (9, 'dup')` over an existing key 9 failed, but row 1 stayed and was flushed with the next statement; sqlite3 undoes the whole statement. The pager's pending writes are now discarded when a statement fails in autocommit mode.
- **A whole-number REAL was stored as a REAL under INTEGER or NUMERIC affinity.** `INSERT INTO t(k INTEGER) VALUES ('12.0')` stored `12.0` where sqlite3 stores the integer `12`, so the key differed in a `PRIMARY KEY` and in the file. Affinity now narrows a REAL that is a whole number and fits an `i64`.

## [0.167.2] - 2026-10-22

### Fixed

- **Emptying an index left a file sqlite3 calls malformed.** Deleting rows from an indexed table (a `CREATE INDEX`, or the implicit index of a `PRIMARY KEY` or `UNIQUE` column) left every index page that lost its last entry in the b-tree, and SQLite refuses to open a page with no cells: after `DELETE FROM big` over 3000 rows, `PRAGMA integrity_check` in sqlite3 answered `database disk image is malformed (11)` (partial deletes that emptied a leaf did the same). A page that is emptied is now taken out of its parent and freed. The parent entry beside it is a real index entry, not just a routing key, so it is put back into the neighbouring subtree rather than dropped; an interior page left with no entries is replaced by its only child, and a root left that way takes the child's place. The pages go back to the freelist, which they never did.
- **Deleting an entry that sits in an interior page could fail with `cells do not fit in page`.** The entry is replaced by its in-order predecessor, and a predecessor longer than the entry it replaces can overflow the page (found by a randomized test with keys of mixed length). The page is now split as an insert would split it. The replaced entry's overflow chain, which leaked, is freed.
- **`PRAGMA integrity_check` reported a freelist mismatch that was not there.** It compared the freelist chain with the page count and freelist length of the header the connection opened with, and the pager changes both as pages are freed; it now reads page 1 as it is.

Checked by a randomized test against a sorted model (1,500 seeds, mixed key sizes, no page leaked) and by engine tests that run a delete and have sqlite3 check the file after every statement; three of those fail on the previous release.

## [0.167.1] - 2026-10-22

### Fixed

- **A SQL query on a `WITHOUT ROWID` table found no rows** (#722). `SELECT count(*)` answered `0` and every other query came back empty, on a file sqlite3 had written with 500 rows in it. The table's b-tree is an index b-tree whose entries are whole rows (the `PRIMARY KEY` columns first, in key order, then the others), and every table scan read it as a table b-tree and silently saw nothing. The cursor factory now recognises such a root (an index page, with a `WITHOUT ROWID` `CREATE TABLE` in `sqlite_master`) and hands back a cursor that reads it as a table: `Column` takes the declared column index, whatever the key is, and there is no rowid. Verified against sqlite3 over a 3000-row table keyed on its last two columns, a table whose every column is in the key, joins, subqueries, aggregates and `ORDER BY`/`LIMIT`.
- **A secondary index on a `WITHOUT ROWID` table is not used to fetch rows.** Its entries end in the key columns, not a rowid, so the index walks, which fetch the row by rowid afterwards, answered `WHERE n = 40` with no row. Such tables enter a `SELECT`'s catalog without their secondary indexes and are scanned; `DROP TABLE` and the other DDL still see them.
- **`PRAGMA integrity_check` and `quick_check` reported a valid `WITHOUT ROWID` file as corrupt** (`unexpected b-tree page type`, and a `rowid not present` line per secondary-index entry). They now walk such a table as an index b-tree, checking that its primary keys strictly increase, and check a secondary index's ordering and entry count.
- **`ANALYZE` failed on any database holding a `WITHOUT ROWID` table.** Such tables are skipped, so the rest are still analyzed.

Writing to `WITHOUT ROWID` tables is still refused; that follows in the next releases (#722).

## [0.167.0] - 2026-10-22

### Added

- **`BETWEEN` and `NOT BETWEEN` in the column and stream engines** (#717, second of milestone 2 of #678). The lowering pass of #716 now also rewrites `x BETWEEN lo AND hi` to `x >= lo AND x <= hi` and `x NOT BETWEEN lo AND hi` to `x < lo OR x > hi`, before validation, so the planner and the VM are unchanged. It applies in WHERE, HAVING, the select list and aggregate and function arguments. Bounds and subject may be expressions, and a quoted number against a number column converts as DuckDB does (#679). The three-valued logic of #679 gives DuckDB's NULL behaviour: a NULL subject or bound makes `BETWEEN` NULL, and `NOT BETWEEN` is TRUE when the other bound already decides (`5 NOT BETWEEN NULL AND 3`). Reversed bounds match nothing. The subject is repeated once per comparison, which changes no answer while every scalar function the subset allows is deterministic; a function like `random()` or `now()` would need its subject refused. A new sqllogictest file (`tests/sqllogictest/column/between_717.test`, 25 queries) and seven `check_column_oracle.sh` cases agree with DuckDB.

## [0.166.0] - 2026-10-22

### Added

- **`IN (list)` and `NOT IN (list)` in the column and stream engines** (#716, first of milestone 2 of #678). The parser lowers `x IN (a, b, c)` to `x = a OR x = b OR x = c` and `x NOT IN (a, b, c)` to `x <> a AND x <> b AND x <> c` before it validates the query, so the planner and the VM are unchanged. It applies in WHERE, HAVING, the select list, an aggregate's argument and a function's argument. The list items may be expressions (`id IN (3, 1 + 1)`), and a quoted number against a number column converts as DuckDB does (#679). The three-valued `OR` and `AND` of #679 give DuckDB's NULL behaviour: `x IN (1, NULL)` is TRUE or NULL, `x NOT IN (1, NULL)` is never TRUE, and a NULL `x` is NULL. An empty list is refused, as DuckDB does, and `IN (SELECT ...)` is unchanged. The cost is a comparison per item per row; a hash-set opcode waits for a benchmark that needs it. A new sqllogictest file (`tests/sqllogictest/column/in_list_716.test`) and six `check_column_oracle.sh` cases agree with DuckDB.

## [0.165.0] - 2026-10-22

### Added

- **`INSERT ... ON CONFLICT ... DO UPDATE`** (#696, second of three steps). A row that conflicts on the constraint a clause names, or on any constraint a target-less last clause answers, updates the existing row instead of being inserted: `DO UPDATE SET column = expr, ... [WHERE expr]`. A bare column in the `SET` or `WHERE` is the existing row's; `excluded.<column>` is the row that would have been inserted, with the column's affinity applied and its real rowid in an `INTEGER PRIMARY KEY` column. A `WHERE` that is false leaves the row alone and inserts nothing. The update goes through `UPDATE`'s own row body, so the new values meet the same `NOT NULL`, `CHECK` and `UNIQUE` checks (a key set to a value another row holds fails with `UNIQUE constraint failed`, setting a key to what it already is does not) and every index stays in step; a `SET` that reassigns the rowid moves the row. Clauses chain by target as in sqlite3, the first one that matches a constraint answering it, and a new row's own `NOT NULL`/`CHECK` violation still aborts before any conflict is looked at. Works over `VALUES` and `INSERT ... SELECT`, a row at a time. `WITHOUT ROWID` writes are still refused (the next step).

## [0.164.0] - 2026-10-22

### Changed

- **`CREATE TABLE ... WITHOUT ROWID` is refused** (#696). The statement was accepted but no `INSERT`, `UPDATE` or `DELETE` could write the table, so it could only be created and never filled. It now fails with `unsupported: WITHOUT ROWID tables are not supported yet`, after sqlite3's own definition errors (`PRIMARY KEY missing`, `AUTOINCREMENT not allowed`) so those messages are unchanged. A `WITHOUT ROWID` table in a file sqlite3 wrote still opens and reads; writes to it keep failing at compile time. Writing to such tables is tracked separately.

## [0.163.0] - 2026-10-22

### Added

- **`INSERT ... ON CONFLICT ... DO NOTHING`** (#696, first of three steps). The parser reads sqlite3's whole upsert clause: `ON CONFLICT [(column, ...) [WHERE expr]] DO NOTHING` or `DO UPDATE SET ... [WHERE expr]`, chained as sqlite3 chains them (only a clause with a target may be followed by another), after a `VALUES` or `SELECT` source; `Insert` gains `upsert` and the AST gains `UpsertClause`, `UpsertTarget` and `UpsertAction`. `DO NOTHING` skips a row that conflicts on the constraint its target names (the rowid, or a `PRIMARY KEY`/`UNIQUE` index over the same columns in any order), or on any uniqueness constraint when it has no target. A conflict on another constraint, and every `NOT NULL` or `CHECK` violation, still aborts, unlike `OR IGNORE`. A target that matches no constraint is refused with sqlite3's `ON CONFLICT clause does not match any PRIMARY KEY or UNIQUE constraint`. `DO UPDATE` parses but is refused as `Unsupported` until the next step, and `WITHOUT ROWID` writes until the one after.

## [0.162.0] - 2026-10-22

### Added

- **`BatchEngine::from_bytes(table, bytes)` opens a Parquet file from memory** (#707). A host with no filesystem, such as a browser running db-core on `wasm32-unknown-unknown`, has the file's bytes but no path for `Engine::open`. The constructor wraps them in `MmapRegion::Owned` and shares the rest of `open`; `table` is the name queries use in `FROM`.

## [0.161.0] - 2026-10-22

### Fixed

- **`PRIMARY KEY` and `UNIQUE` constraints write a valid file and are enforced** (#694). A `PRIMARY KEY` on a non-`INTEGER` column, a composite `PRIMARY KEY` and a `UNIQUE` constraint define an implicit index, `sqlite_autoindex_<table>_<n>`. `CREATE TABLE` never created it, so sqlite3 reported the file as malformed (`PRAGMA integrity_check`) and a duplicate key was accepted. It now allocates each index's b-tree and writes its `sqlite_schema` row with a `NULL` `sql`, numbered and de-duplicated as sqlite3 does (a rowid alias needs none; a `WITHOUT ROWID` table's key is the table). The catalog reads these rows back from files sqlite3 wrote, so `INSERT`, `UPDATE` and `DELETE` keep them consistent and `DROP TABLE` frees them. `CREATE TABLE ... WITHOUT ROWID` now allocates an index b-tree for its root, so the empty table is a valid file (writes to it are still unsupported, #696). `DROP INDEX sqlite_autoindex_*` is refused as sqlite3 refuses it.
- **A `UNIQUE` index is enforced on `UPDATE`, and a `NULL` key never conflicts.** `UPDATE` accepted a key that duplicated another row; it now fails with `UNIQUE constraint failed`, an unchanged key is not a conflict with itself, and `UPDATE OR IGNORE` skips the row. `UPDATE OR REPLACE` on a table with a touched unique index is refused (it would have to delete the other row), not left unenforced. `INSERT` rejected a second row with a `NULL` in a unique key; sqlite3 allows it. The error names the columns (`UNIQUE constraint failed: t.a, t.b`), not the index.
- **A table-level `PRIMARY KEY (c)` over an `INTEGER` column is the rowid in a table of any width.** It was recognized only in a one-column table, so in `CREATE TABLE t (a INTEGER, b, PRIMARY KEY (a))` the column read as `NULL`.

### Changed

- **Writes to a table with a non-`BINARY` index key are refused** (`Unsupported`). The index b-tree orders keys byte for byte, so a `COLLATE NOCASE` or `RTRIM` key was written out of order and a uniqueness probe missed case variants. Reads and updates that leave such a key alone are unaffected.
- **Public API:** `P4::CreateTable` gains `without_rowid` and `autoindexes`; `SchemaStorage` gains `insert_autoindex_row` (the default refuses, so a storage without it fails `CREATE TABLE` instead of writing a malformed file); `storage::row::btree::insert_autoindex_master_row` is new.

## [0.160.0] - 2026-10-22

### Changed

- **`AND` conjunctions and BLOB comparisons use an index** (#695). A `WHERE` of several `AND`-ed terms plans as one index `SEARCH` when equality terms cover a prefix of an index's columns, optionally followed by a range (`<`, `<=`, `>`, `>=` or `BETWEEN`) on the next column: `a = ? AND b = ?`, `a = ? AND b = ? AND n >= ? AND n < ?`, `n >= ? AND n < ?`. The terms the walk does not enforce run as a per-row filter (a residual term may not carry a bind parameter, otherwise the query keeps its scan). Without an equality prefix only a range with both bounds seeks. `UPDATE` and aggregate scans take the same walk when it enforces the whole `WHERE`. BLOB literals are indexable for `=`, `<`, `>` and `BETWEEN`. `EXPLAIN QUERY PLAN` words it as sqlite3 does, e.g. `(a=? AND b>?)`.

## [0.159.1] - 2026-10-22

### Fixed

- **A REAL becomes text the way sqlite3 3.53 does** (#652): `%!.17g` through `sqlite3FpDecode`, whose double-double arithmetic approximates past about 16 digits, where db-core followed 3.51's `%!.15g`. `CAST(1e15 AS TEXT)` is `1000000000000000.0` (was `1.0e+15`), `1.0/3` is `0.33333333333333332`, `123456789012345678.0` is `1.2345678901234568e+17`, `printf('%!.17g', 0.000123456)` is `0.000123456`, an infinity is `Inf` and negative zero `0.0`. This covers `CAST`, `||`, the text functions, result display and the shell modes. `printf`'s `%f`, `%e` and `%g` use the same decoder, including its rounding of exact ties (`printf('%.0e', 450.0)` is `4e+02`) and `!` past 18 digits (up to 26), and `#` drops the sign of a negative value that shows as zero. `tools/check_row_functions_oracle.py` gains a REAL-to-text section and `--random N [SEED]`, a randomized byte-for-byte comparison of REAL-to-text and `printf`, run by `make check-row-functions-oracle`.

## [0.159.0] - 2026-10-22

### Changed

- **A filtered projection skips a row group's projection-only columns when no row survives** (#704). ADR-0026's phase 2b used to decode those columns at the surviving positions even when there were none, reading and decompressing every page for a row group the filter had emptied. Now each one is added as an empty column without opening its pages. In t-rust-db/examples `parquet-codecs` (2M rows, matches in 3 of 14 row groups), through column-rs: `SELECT id, user_name … WHERE note LIKE '%request 1999%'` runs 0.3–1.8 ms faster and uses 4–9 ms less CPU.

## [0.158.0] - 2026-10-22

### Added

- **`engine::column::file_segments`** (#699): a Parquet file's row groups as late-materializing `FileSegment`s, for callers that run the batch engine themselves over a file, as column-rs does (t-rust-db/tools#62). It works like `snapshot_segments` does for table directories. A filtered query decodes its other columns only at the matching rows (ADR-0026), and a top-N fetches only its winning rows (#513). `t.col` names the column `col`, and batches are keyed by the name as given.

### Changed

- **A late-materialized dictionary string column decodes only the entries it returns** (#699). `decode_column_at` used to decode a row group's whole dictionary into `String`s, copy each into an `Arc<str>`, and only then gather the codes it needed. `RowGroupReader::read_string_column_dictionary_at` gathers the codes first and decodes only the entries they use, skipping the rest by their length prefix. In t-rust-db/examples `parquet-codecs` (2M rows), through column-rs with tools#62: `SELECT id, user_name … WHERE note LIKE '%request 1999%'` used to cost 10–33 ms more than `SELECT id`, and now costs 1.6–2.5 ms more (46.0 -> 31.7 ms on DuckDB's zstd file).

## [0.157.0] - 2026-10-22

### Changed

- **zstd pages are decoded by a decoder of our own** (#700), replacing ruzstd's (ruzstd stays as the encoder until #692). An RFC 8878 decoder writes each page straight into an output buffer sized from the page header, with 16-byte wild copies. It checks the frame's content checksum (XXH64), and refuses a declared size above 32 KiB per compressed byte, blocks or literals over 128 KiB, sequence counts the output cannot hold, and dictionary frames. The error contract (`ZstdError::Frame`, `Read`, `SizeMismatch`) is unchanged. Decode throughput is about 2x ruzstd on db-core-written pages and 1.5x on DuckDB's. A full-scan free-text search over 2M rows (t-rust-db/examples `parquet-codecs`) runs 32.1 -> 29.6 ms on db-core's zstd file and 29.2 -> 26.8 ms on DuckDB's; zstd's cost over the uncompressed file drops by about 36%.

## [0.156.0] - 2026-10-22

### Changed

- **`LIKE`, `ILIKE` and `GLOB` over a string column compile their pattern once** (#691). A constant pattern whose only wildcard is "any run" (`%`, or `*` for `GLOB`) becomes literal segments, matched with `starts_with`, `find` and `ends_with` on each row's bytes in the typed column's own buffer: no `Value`, `String` or `Vec<char>` per row, and no backtracking. A dictionary column matches each distinct value once. Patterns with `_`, `?` or `[...]`, and non-literal patterns, take the general matcher as before. A full-scan free-text search over 2M rows (t-rust-db/examples `parquet-codecs`): `count(*) … LIKE '%host 42%'` 95 -> 24 ms (DuckDB 22 ms), and with a projection and `ORDER BY` 110 -> 40 ms (DuckDB 22 ms).

## [0.155.0] - 2026-10-21

### Added

- **zstd page compression in the Parquet writer** (#622): `Compression::Zstd`, at `ruzstd`'s one implemented level (`Fastest`, about zstd level 1), pure Rust. The codec is recorded per column chunk, so a table that changes `TableOptions.parquet.compression` reads its old files as they are, and its day merge writes the new codec. Snappy stays the default. DuckDB and pyarrow read every writer type's zstd pages (`make check-parquet-write-oracle`, 4 new cases). `bench-events` takes `CODEC=snappy|zstd|uncompressed`. On its 10M-event workload, against Snappy: 8.2 vs 10.8 bytes/event (−24%), hour flushes +10%, the day merge +26%, and hot-cache reads 4-13% slower (user CPU +14-16%); uncompressed is 13.4 bytes/event.

## [0.154.0] - 2026-10-21

### Fixed

- **A small Parquet file can no longer expand past what its metadata states** (#671). Every page of a column chunk is checked before it is decompressed: a data page's `num_values` against what the row group's rows leave (a nested leaf: the chunk's `num_values`), and every page's `uncompressed_page_size` against what the chunk's `total_uncompressed_size` leaves. A page that claims more is `FileError::PageExceedsChunk`. This closes RLE and zero-bit-width runs that expanded to `i32::MAX` values, and ZSTD pages that grew to their header's size (the 64 MiB cap of #657 bounded only the first allocation).
- **Dictionary strings are decoded once, not per row** (#671, #686). The engine's late-materialization path gathers dictionary codes into a dictionary column instead of decoding the whole chunk, and a writer's `PLAIN` fallback page after a dictionary extends the dictionary rather than dropping to a per-row copy. Nested and `MAP` string leaves share one allocation per dictionary entry.

### Changed

- **Breaking:** `LeafScalar::Str` holds an `Arc<str>`, and `MapEntries` (the Parquet reader's, the writer's `ColumnValues::Map` and the VM's `Value::Map`) is `Vec<(Arc<str>, Option<Arc<str>>)>`. Build them with `.into()` from a `&str` or `String`; `RecordValue::Map` keeps owned strings.

## [0.153.0] - 2026-10-21

### Added

- **Queries skip files and row groups on the sort key** (#589). A tiered table's manifest entries record each sort-key column's min/max (integer, timestamp and UTF-8 columns; manifest version 4, written only when a file has ranges, so versions 1-3 read as before), and a `WHERE` clause drops the files whose ranges cannot match before their footers are opened. A row group whose min/max covers an equality literal is still skipped when its dictionary lacks it: the dictionary page is decoded without the data pages, and only when every data page is dictionary-encoded. A range or dictionary that cannot be read means "may match", never a wrong answer.

## [0.152.0] - 2026-10-21

### Changed

- **`LoadConst` no longer copies its literal into every row** (#681). A constant register holds one value and the row count it stands for. Comparing a dictionary column with a string literal (`name = 'search'`) reads that value directly; any other opcode that reads the register row by row gets the broadcast column when it first needs it. Filters and joins change only the stored row count. `cross` (10M events): 211 -> 164 ms in-process, user CPU 991 -> 491 ms, same output as before. `vm_opcodes` A/B against v0.151.0 shows no regression beyond noise.

## [0.151.0] - 2026-10-20

### Fixed

- **Eight silent wrong answers in the column engine** (#679, milestone 1 of #678), each now checked against DuckDB 1.5.5, the column engine's reference:
  1. **AND/OR with NULL** follow three-valued logic: `TRUE OR NULL` is TRUE and `FALSE AND NULL` is FALSE. `WHERE n > 0 OR d > 100` used to drop rows where `d` is NULL; NOT, HAVING and the stream engine were affected the same way.
  2. **OFFSET is applied.** It was accepted, then ignored, on every path: the top-N, a multi-key sort, grouped queries and the bounded scan.
  3. **`IN (SELECT ...)` or `EXISTS` inside a larger condition is refused** with an error. It used to compile to FALSE.
  4. **A quoted value compared with a number or boolean column is converted to that type**, as DuckDB does: `id = '5'` matches row 5, and `amount > '100'` compares numbers. A value that does not convert is an error (new `VmError::Conversion`). Before, the comparison never matched, and `<>` always did.
  5. **A TIMESTAMP column compared with a quoted date or timestamp compares as a timestamp**, in Parquet files and record tables: `ts >= '2026-01-02'` used to match nothing. A quoted value that is no timestamp is an error. Inside an aggregate (`max(ts) > '...'`) the comparison is an error until the timestamp type lands (#678 milestone 3).
  6. **`LIKE` is case-sensitive**, as in DuckDB; it used sqlite3's case-insensitive matcher.
  7. **`regexp_extract` returns `''`** when nothing matches, a group takes no part in the match, or the group number is out of range, as in DuckDB. NULL comes only from a NULL argument.
  8. **A window query's WHERE filters the rows before the windows**, instead of being dropped (column-rs; `codegen::batch::compile_window`).

### Added

- **`ILIKE` / `NOT ILIKE`** in the column dialect: case-insensitive LIKE, with Unicode lowercasing. It is a token only in the column dialect, so sqlite3's grammar and the row engine are unchanged.
- **ADR 0030:** the column dialect follows DuckDB where DuckDB and sqlite3 differ (amends ADR 0011).
- **Tests:** `unit_column_wrong_answers` runs every case as SQL through `BatchEngine::run_query` on the new `null_types.parquet` fixture (NULLs, mixed-case text, a decimal and a timestamp column), and each step has unit tests. `make check-column-oracle` has 22 more cases.

### Changed

- **Behaviour:** `LIKE` in the column and stream engines is case-sensitive. A filter such as `msg LIKE '%error%'` no longer matches `ERROR`; use `ILIKE`.
- **Public API (breaking):**
  - `Opcode::Limit` gains `offset`;
  - `MapOp::Like` gains `nocase`;
  - `VmError::Conversion` and `TokenKind::Ilike` are new variants.
  - Code that builds or matches these needs updating.

## [0.150.0] - 2026-10-19

### Added

- **`ORDER BY` with several terms in the column and stream engines and in emitted programs** (#645). `ORDER BY a, b DESC` sorts by each term in turn. NULLs come last for every term in either direction, as in DuckDB.
- **A query that neither groups nor aggregates may sort on a column it does not select.** The column is emitted after the select list and dropped after the sort and `LIMIT`.
- `make check-column-oracle` has four multi-term cases against DuckDB. `make bench-events` replays sessions with `ORDER BY session_id, request_id`.

### Changed

- **`Opcode::Sort` is `{ keys, hidden }`** instead of `{ col, descending }`.
  - `keys` lists `(output column, descending)` pairs, most significant first.
  - `hidden` counts the trailing sort-only columns.
  - `vm::engine::finalize` takes the keys as `&[(usize, bool)]`, and the new `vm::batch::compare_rows_for_order` compares two rows by them.
  - Code that builds or matches `Opcode::Sort`, or calls `finalize`, needs updating.
- **`ORDER BY ... LIMIT` with several terms sorts the whole result before limiting it.** The parallel top-N path still takes a single key.

### Fixed

- **An `ORDER BY` term the planner could not place no longer drops the sort silently.** Such terms were a column outside the select list, and a `COUNT(DISTINCT x)` key whose label differed from its select item. Unsorted rows came back with no error.
  - A non-selected column now sorts as a hidden key.
  - An aggregate key is matched to its select item structurally.
  - What remains unsupported is refused: in a grouped or aggregate query, a key that is not in the select list; with `SELECT DISTINCT`, a key outside the list.

## [0.149.0] - 2026-10-19

### Added

- **`HAVING` in the column engine's SQL subset** (#644). It filters the merged groups before `DISTINCT`, `ORDER BY` and `LIMIT`. It may read:
  - `GROUP BY` keys;
  - the select list's aggregates and its aliases (`count(*) AS n ... HAVING n > 1`);
  - aggregates the select list does not show, which are merged as extra columns that never reach the output;
  - `COUNT(DISTINCT x)` when the select list has it too.

  Without `GROUP BY`, the query's one group can be filtered out, leaving no rows. It works the same over a cross-mode join. `HAVING` runs as a `Filter` in `Combine`'s post-aggregation projection (#623), so EXPLAIN shows it there.
- **Refusals, worded as for the select list:**
  - a column that is neither a key nor aggregated;
  - `HAVING` without `GROUP BY` and without an aggregate;
  - `HAVING` alongside a window function;
  - a `COUNT(DISTINCT ...)` in `HAVING` that is not in the select list (a query has one distinct key).
- `make check-column-oracle` has nine `HAVING` cases against DuckDB.
- `make bench-events` runs the A-and-B sessions query with `HAVING count(DISTINCT name) = 2` on every engine, instead of filtering afterwards.

## [0.148.0] - 2026-10-19

### Changed

- **Typed `GROUP BY` accumulators in the cross-segment merge** (#643). `Combine` keeps each group's key as an 8-byte integer, or as a 4-byte code into the column's own string table, instead of a 32-byte `Value` plus a heap string. `COUNT` slots are 8-byte integers too. Worker partials stay typed into the final merge, so a typed partial is never turned back into `Value`s.
  - `COUNT(DISTINCT)` frees its `(keys, x)` index before regrouping by the real keys.
  - `GroupReduce` builds its key columns directly, instead of a `Vec<Value>` per group that was then copied into columns.
  - Measured on 10M events (`make bench-events`, `cross`: sessions with events A and B, 504k groups):
    - live heap peak 100 → 78 MB on 1 thread and 104 → 89 MB on 12;
    - peak RSS 344 → about 290 MB, against DuckDB's 159 MB.
  - Results and group order are unchanged.
  - The rest of the RSS gap is allocator retention: the macOS allocator, and mimalloc likewise, keep the freed blocks of vectors that grew while groups accumulated, about 10 MB per worker thread. It is not live db-core data. #643 stays open for the remaining steps.

### Fixed

- **`make bench-events` no longer reports a false mismatch with sqlite3 3.53** (#643). Its `-csv` output ends lines with CRLF, so the comparison now strips the CR.

## [0.147.0] - 2026-10-19

### Added

- **sqlite3's built-in scalar functions in the row engine** (#651), each checked against sqlite3 3.53.4:
  - text and core: `char`, `concat`, `concat_ws`, `format`/`printf`, `octet_length`, `random`, `randomblob`, `substring`, `unicode`, `unistr`, `likely`, `unlikely`, `likelihood`, `sqlite_source_id`, `sqlite_compileoption_used`, `sqlite_compileoption_get`, `sqlite_offset`;
  - connection state: `changes`, `total_changes`, `last_insert_rowid`, answered by the VM (new `vm::row::ConnectionState`, `Vm::set_connection_state`, `Vm::connection_state`);
  - math: `acos` ... `trunc`, `log` with one and two arguments;
  - date and time: `date`, `time`, `datetime`, `julianday`, `unixepoch`, `strftime`, `timediff`, with every modifier (`localtime` and `utc` take the local zone as UTC until #653); `'now'` is one instant per statement;
  - JSON: `json`, `json_array`, `json_array_length`, `json_error_position`, `json_extract`, `json_insert`, `json_object`, `json_patch`, `json_quote`, `json_remove`, `json_replace`, `json_set`, `json_type`, `json_valid`, with JSON5 input and JSONB blobs read.
- **sqlite3's JSON subtype, decided at compile time.** `json_array(json('[1]'))` nests `[[1]]` while `json_array('[1]')` quotes `["[1]"]`, through `CASE`, `iif`, `coalesce`, `nullif`, `min`/`max`, `COLLATE` and scalar subqueries. An argument whose subtype would depend on the row where it is also compared (`nullif(json_extract(...), '')`) is refused rather than guessed.
- `make check-row-functions-oracle` runs `tools/row_functions_cases*.sql` through the row engine and sqlite3 and writes the agreed answers to `.expected` files; the new `unit_row_functions_oracle` test holds the row engine to them in CI.
- New error variant `FunctionError::Sql` carries sqlite3's own message (`malformed JSON`, ...). A client matching `FunctionError` exhaustively needs an arm for it.

### Changed

- **The row engine's `json_extract` is sqlite3's**: it returns objects, takes several paths, reads JSON5, and raises `malformed JSON` instead of returning NULL. The batch engine keeps #307's late-parse `json_extract`.
- **`iif`/`if` take sqlite3's variadic form** `iif(B1, V1, ..., [E])` and compile as `CASE`; `iif`, `if`, `coalesce` and `ifnull` evaluate lazily, so an argument past the one that decides never runs or raises.
- **One function registry**: `functions::SCALAR_FUNCTION_NAMES` lists every callable function, and a name not in it is "no such function" when the statement is prepared (`soundex`, which sqlite3 3.53.4 leaves out, included).

### Fixed

- `length(X)` counts the characters before the first NUL, as sqlite3 does.
- `printf` widths and precisions wrap to 31 bits like sqlite3's; a width, or a digit or `%c` precision, past `MAX_LENGTH` is "string or blob too big" before any output is built, and so is `randomblob(N)` past it. A `%f`/`%e`/`%g` precision is capped at 100,000,000.

## [0.146.0] - 2026-10-18

### Fixed

- **Corrupt Parquet files return errors instead of panicking** (#657). The Parquet reader has met the crate-wide lint bar since this release: no unchecked arithmetic, indexing or casts, and no module-level allows. Crafted or damaged input that used to panic now gets an error:
  - footer and schema: an empty schema, `num_children` past the element count or `i32::MAX`, deep schema chains, deep Thrift nesting, and huge decimal scales;
  - RLE/bit-packed and delta encodings: bit widths over 32/64, run-length overflow, and huge header counts or block sizes;
  - nested columns: level or value streams shorter than the rows need;
  - timestamps: `TIMESTAMP_MILLIS` and INT96 values outside `i64` microseconds, which give the new `FileError::TimestampOutOfRange`.
- **Header fields no longer drive up-front allocations** (#657). Snappy's buffer comes from the block's declared length, capped at 22× the compressed bytes (the most any op expands to). ZSTD's up-front buffer is capped at 64 MiB, and page assembly no longer reserves `num_values` slots taken straight from the header. Delta streams with a block size above 65,536, or claiming more values than their page, are rejected, and so is a `FIXED_LEN_BYTE_ARRAY` column with `type_length` 0. Schema leaf paths are capped at 64 MiB in all (`schema_tree::leaf_paths` now returns `Option`). A run-length or zero-width stream can still legitimately expand to the page's `num_values` (up to `i32::MAX`).
- New error variants: `EncodingError::InvalidBitWidth` and `NestedError::TruncatedLeafData`.
- A mutation test (`unit_parquet_file_corruption`) and per-decoder fuzz tests run hundreds of mutations of every fixture through every reader and fail on any panic.

## [0.145.0] - 2026-10-18

### Changed

- **Decimal overflow has its own error** (#660). A decimal `SUM` or decimal arithmetic past DECIMAL(38) is now `VmError::DecimalOverflow` ("decimal overflow (past DECIMAL(38))"), not "integer overflow". A client that matched `VmError::IntegerOverflow` for decimal overflow must also match the new variant.

### Fixed

- **Stale docs** (#660). `storage::row::vfs` no longer claims `unsafe` has "no local override anywhere": `fcntl` has ADR 0016's documented carve-out. A stale `#[allow(unused_imports)]` on an import that is in use is gone.

## [0.144.0] - 2026-10-18

### Fixed

- **`DROP VIEW` (and `DROP TABLE`/`DROP INDEX`) delete the `sqlite_master` row by type and name** (#658). `btree::delete_master_row` and `SchemaStorage::delete_master_row` take a `kind` and match the type too, so a same-named object of another kind is never the row removed (a trigger named like a view, in a file sqlite3 wrote). A `SchemaStorage` implementation outside the crate needs the new `kind` argument.
- **No memory is leaked per error** (#658). `BtreeError::MasterEntryNotFound` and `BtreeError::InvalidRootPage` carried a name leaked with `Box::leak`. They now carry no name: `BtreeError` stays free of drop glue for the hot seek path (#679), and the row engine's storage adapter, which knows the name, adds it to its message.
- **`zeroblob(N)` past 1e9 is "string or blob too big"** (#659), as in sqlite3, instead of a clamped 1 GB allocation. The limit is the new public `functions::MAX_LENGTH`, sqlite3's `SQLITE_MAX_LENGTH`, and the error is the new `FunctionError::TooBig`. The row engine reports it in sqlite3's words (new `ExecError::TooBig`).

## [0.143.0] - 2026-10-18

### Fixed

- **Deeply nested JSON no longer crashes the process** (#656, security). The JSON parser (`json_path`, used by `json_extract` and by JSON Lines ingest) recursed once per object level with no cap, so a document nested about 50,000 levels deep overflowed the stack. That included one hostile log line at ingest.
  - Nesting is now capped at sqlite3's `JSON_MAX_DEPTH` of 1000 (`json_path::MAX_DEPTH`).
  - A deeper document is malformed: `json_extract` returns NULL, and an ingested line is kept whole as its message, as any malformed line is.
  - Documents a million levels deep are refused cleanly.
  - A table's `manifest.json` refuses arrays nested more than 8 deep, where objects inside arrays used to recurse without a cap. Real manifests nest one or two.

## [0.142.0] - 2026-10-18

### Fixed

- **A long operator chain no longer crashes the process** (#655, security). Before, `SELECT 1+1+...` or `WHERE a AND b AND ...` with about 10,000 terms overflowed the stack and aborted the whole process. The parser's `MAX_EXPR_DEPTH` (200) bounds its own recursion only, while a flat chain is parsed by a loop into a left-deep tree as tall as the chain, and validation, planning, the VMs and `Drop` all recurse over that tree.
  - The parser now caps expression height at sqlite3's `SQLITE_MAX_EXPR_DEPTH` of 1000, with sqlite3's message, `Expression tree is too large (maximum depth 1000)`. sqlite3 3.53.4 accepts the same boundary: 1000 `+` terms, 999 `AND`-ed comparisons.
  - The cap covers every flat loop: AND/OR, comparisons and postfix operators, arithmetic and `||`, `COLLATE`, and column-dialect subscript chains (`m['a']['b']...`).
  - As in sqlite3, parentheses are not a level.
  - Each operand is measured once, so a very wide operand (a 100k-element `IN` list) costs no more than it did before.
  - Once a statement is too tall the parser builds nothing taller, so a 100k-term chain is refused in milliseconds.
  - A release build runs an expression at the limit in under 1 MiB of stack.

## [0.141.0] - 2026-10-18

### Changed

- **Lower peak memory for scans and aggregates** (#643). Measured on 10M events (`make bench-events` data, macOS peak footprint):

  | query | before | after | DuckDB |
  |---|---:|---:|---:|
  | `count(*) WHERE day = X` | 97 MB | 41 MB | |
  | `count(*) WHERE name = 'a' OR name = 'b'` | 167 MB | 54 MB | |
  | sessions with events A and B (`COUNT(DISTINCT)`, 504k groups) | 586 MB RSS | 292 MB (324 MB RSS) | 140 MB (154 MB RSS) |
  | `GROUP BY session_id` (648k groups) | 400 MB | 338 MB | 102 MB |

  The changes:
  - **A segment's body runs in slices of `SLICE_ROWS` (16,384) rows** (for filters, expressions and aggregates; `ORDER BY ... LIMIT` and bare `LIMIT` scans still read a segment whole). The VM materializes its registers row by row, so a worker used to hold a whole row group's worth of every register: up to `row_group_max_rows`, 1M by default. It now holds one slice's worth, so memory no longer grows with the row-group size. Only bodies of row-wise and partial-aggregate opcodes slice; joins, windows and sources still see the segment whole. Partials merge as per-segment ones do, and group order is unchanged.
  - **New crate-internal helpers:** `Column::slice` and `Batch::slice`.
  - **A segment's slices fold into one partial before the merge**, on the path that does not pre-aggregate per worker, so the merge never sees a group once per slice.
  - **The final merge sizes its accumulators once**, frees each worker's partial as soon as it is merged, and orders groups in place instead of copying the columns.
  - **The generic group-key index keeps one map entry per group** instead of allocating a `Vec` for each.

### Known limits

- High-cardinality `GROUP BY` still uses 2-3x DuckDB's memory. Group keys and accumulators are 32-byte `Value`s; typed accumulators are a follow-up.
- `... WHERE ... LIMIT n` without `ORDER BY` still collects every matching row before cutting to `n`.

## [0.140.0] - 2026-10-18

### Fixed

Row engine parity with sqlite3 3.53.4. On `make fuzz-diff TARGET=row N=2000 SEED=1`, wrong answers go from 18 to 0 and over-permissive findings from 54 to 6.

- **`DROP VIEW`** (#552). It was parsed but refused as "unsupported or unrecognized statement". It now removes the view's `sqlite_master` row, found by its stored name in any case (new `Opcode::DropView`, `P4::DropView`, `compile_drop_view`). On a table it says `use DROP TABLE to delete table t`, even with `IF EXISTS`. An unknown name is `no such view: x` unless `IF EXISTS`.
- **`PRAGMA journal_mode = WAL|DELETE` answers with the mode it set** as one TEXT row (`wal`, `delete`), as sqlite3 does, when a pager applied it (#561).
- **`PRAGMA synchronous = N` inside a transaction is an error**: `Safety level may not be changed inside a transaction` (new `ExecError::SynchronousChangeDuringTransaction`, #563). Changing the level anyway was also why a later `PRAGMA synchronous` disagreed with sqlite3 (#561).
- **`INSERT INTO t (cols) DEFAULT VALUES` is refused**: it supplies 0 values for those columns (#563).
- **Rules sqlite3 checks when it prepares a statement** (#562), with its messages, through the new `CodegenError::Rejected`:
  - **name clashes across kinds:** tables, views and indexes share one namespace, so `there is already a table named t` and `there is already an index named i` are errors even with `IF NOT EXISTS`;
  - **unknown functions:** `no such function: f` or `wrong number of arguments to function f()` at compile time, also where no row would evaluate the call (new `functions::exists`, `functions::prepare_error`). An sqlite3 built-in that db-core doesn't implement yet (`random`, `printf`, `date`, ...) is not refused at compile time; it still fails only when a row reaches it;
  - **`CREATE TABLE` definitions:**
    - duplicate column names;
    - expressions, subqueries or unknown columns in `PRIMARY KEY`/`UNIQUE`;
    - more than one primary key;
    - subqueries, aggregates, unknown functions or unknown columns in `CHECK`;
    - `WITHOUT ROWID` without a primary key;
    - `AUTOINCREMENT` off an `INTEGER PRIMARY KEY` or on a `WITHOUT ROWID` table;
    - `STRICT` columns with a missing or unknown type.
- **`make fuzz-sql`/`fuzz-diff` build again.** The fuzz comparer lacked a `Cell::Decimal` arm after #632.

### Known limits

- An unknown function that codegen never compiles still slips through. Examples: a `GROUP BY` term of `INSERT ... SELECT`, or a `SET` assignment that a later one overrides.

## [0.139.0] - 2026-10-18

### Changed

- **Arithmetic is exact and typed as in DuckDB** (#635). Integer `+`, `-` and `*` compute in 64-bit integers: exact past 2^53 (`9007199254740993 + 2` was off by one), and an overflow is an error (`VmError::IntegerOverflow`, "Map: integer overflow") instead of a saturated value. Decimals stay decimals: `+`/`-` at the larger scale, `*` at the sum of the scales, an integer counts as scale 0 (`price * 2` over DECIMAL(9,2) `1.10` is `2.20`, not the DOUBLE `2.2`), and a result past DECIMAL(38) is an error. A float operand makes the result a float, and `/` is always a float (`7 / 2` is `3.5`, `1 / 0` is `inf`), as in DuckDB. Negating `i64::MIN` is an error too (it became a float, SQLite's way). This holds for row expressions, the typed column fast path and expressions over aggregates (`SUM(price) * 2`). New `make check-column-oracle` cases (63/63 agree with DuckDB).

### Fixed

- **Negating a decimal** (`-price`) returned NULL; it is the negated decimal.
- **A constant after a computed term in a filtered query** (`SELECT id * 3 - 1 FROM t WHERE id > 10`) failed with `RegisterLengthMismatch`: a constant loaded after a `Map` had applied the `WHERE` selection got the batch's length, not the filtered rows'. This bug was already on main.

## [0.138.0] - 2026-10-18

### Fixed

- **`GROUP BY` a column that a select item's alias shadows groups by the column** (#623). `SELECT b - b AS b, count(*) FROM g GROUP BY b` grouped by the alias's expression and returned one group where SQLite and DuckDB return one per value of `b`. A real column now wins; only a name that is no column of the table resolves to a `SELECT`-list alias (#307). `planner::expand_star` does this resolution for one table and the new public `planner::resolve_group_by` for any column list; a caller must run one of them before `planner::compile` for an alias in `GROUP BY` to resolve. Every engine path does: joins resolve against the joined tables' columns. A name that resolves to an aggregate's alias (`SELECT SUM(x) AS s ... GROUP BY s`) is refused, as in DuckDB; it used to group by the aggregate's per-segment value.
- **`LIKE` over an aggregate** (`MIN(region) LIKE 'e%'`) is merged like any other aggregate expression; it failed with "planner invariant violated".
- **An aggregate inside `IN (SELECT ...)`** (`SUM(x) IN (SELECT ...)`) is refused. It was evaluated per row and returned one wrong row per input row.
- **An aggregate over the `GROUP BY` key itself** (`SELECT k, MIN(k) ... GROUP BY k`) failed with `RegisterLengthMismatch` on untyped input: `GroupReduce` wrote the per-group keys back into the key register before reading the aggregates' sources. This bug was already on main.

### Changed

- **Grouped select lists follow standard SQL** (#623). Every column a select item reads outside an aggregate must be a `GROUP BY` key, and that is the only rule: keys may be left out, reordered, repeated, computed on (`SELECT year - 1 AS y, SUM(x) FROM t GROUP BY year`) or grouped by their alias (`SELECT region AS r ... GROUP BY r`), `COUNT(DISTINCT x)` included, and an item may combine keys and aggregates with any operator or function (`region || ':' || COUNT(*)`, `MAX(x) - MIN(x) + COUNT(*)`). A column that is neither grouped nor aggregated is refused with ``column `x` must be a GROUP BY key or inside an aggregate``. Results match DuckDB (`make check-column-oracle`, nine new cases).
- **Arithmetic on aggregates keeps integer results** (#623, the literal half of #635). `SUM(x) * 2` and `SUM(x) + 1` over integers are `BIGINT`, not `DOUBLE`, as in DuckDB. The integer kernel still computes through `f64` (exact up to 2^53); #635 makes it exact.
- **`Opcode::Combine` has a new `project` field**: a post-aggregation program over the merged rows (columns `#0`, `#1`, ...) that computes the select list; empty when the merged columns already are the output. It runs after `output_order` and before `DISTINCT`/`ORDER BY`/`LIMIT`, in every engine path and in AOT-emitted programs. A client that builds `Opcode::Combine` itself needs `project: Vec::new().into()` (or `Cow::Borrowed(&[])`). The planner no longer emits `AggPart::Expr` or `AggPart::Hidden`; both stay in the VM.

### Known limits

- The ahead-of-time emitter (`emit::generate`) refuses `GROUP BY` a `SELECT` alias (other than a column renamed to itself): it plans from SQL alone, with no table schema to tell an alias from a column. Before, it grouped by the alias, which was wrong when a real column had the same name. Grouping by a column, with computed select items, emits as usual.

## [0.137.0] - 2026-10-17

### Added

- **`COUNT(DISTINCT x)` in the batch engine** (#633). It works over Parquet files, table directories, the stream engine and joins, with or without `GROUP BY`, `FILTER` and `ORDER BY COUNT(DISTINCT x)`. The result does not depend on how the input splits into segments and morsels, and matches DuckDB (`make check-column-oracle`). NULLs are not counted, and over no rows the count is 0. It runs as a second grouping: segments group by `(keys, x)`, so merging them removes copies of `x`, and `Combine` then groups by the keys alone. Memory grows with the distinct values per group. A new public `AggPart::DistinctKey` marks the hidden key.

### Known limits

- One `COUNT(DISTINCT ...)` per query, and not inside an expression (`COUNT(DISTINCT x) * 2`) or a window. `SUM`/`AVG`/`MIN`/`MAX(DISTINCT ...)` are refused with a clear error. For high cardinality, use `approx_count_distinct` (#603, planned).

## [0.136.0] - 2026-10-17

### Changed

- **DECIMAL is exact** (#632). A Parquet DECIMAL column reads as a new `Value::Decimal { unscaled: i128, scale: u8 }` (and `engine::Cell::Decimal`), not a rounded `Float`. It prints at its scale, as DuckDB prints it (`12.34`, `-1.500`). `SUM` over decimals is exact at the largest scale seen (`0.10 + 0.20` is `0.30`, not `0.30000000000000004`), and an error (`VmError::IntegerOverflow`) past DECIMAL(38). `MIN`/`MAX` return the decimal; `AVG` is a float. Comparison, `ORDER BY`, `GROUP BY` and join keys work by value across scales (`1.50 = 1.5`). A FIXED_LEN_BYTE_ARRAY decimal wider than 16 bytes is refused (`FileError::InvalidMetadata`) rather than truncated. New public items: `Value::Decimal`, `Cell::Decimal`, `vm::batch::numeric_cmp` and `value::format_decimal`. A client that matches `Value` or `Cell` exhaustively needs a `Decimal` arm.

### Known limits

- Arithmetic on decimals (`price * 2`) is still float (#635). Statistics pruning skips DECIMAL columns, so it reads them in full.

## [0.135.0] - 2026-10-17

### Changed

- **Integer aggregates are exact, and text MIN/MAX works** (#631). `SUM`, `MIN` and `MAX` over integers return exact `Int`s instead of rounded `Float`s: `MAX(session_id)` above 2^53 is no longer off. `MIN`/`MAX` over text return the smallest/largest string instead of NULL. `SUM` over integers that overflows 64 bits is an error (`VmError::IntegerOverflow`), as in SQLite; a float input makes `SUM` a float; `AVG` is always a float. Window `SUM`/`AVG` follow the same rules. A new `VmError` variant and an internal `AggFunc::AvgSum` (not reachable from SQL) are public. Result types change for integer input: a client that matched `Float` for `SUM`/`MIN`/`MAX` must accept `Int`.

### Known limits

- Arithmetic on aggregate results (`SUM(x) * 2`) is still float (#635); the stream engine's `SINCE`-range aggregates stay DOUBLE (#634).

## [0.134.0] - 2026-10-16

### Added

- **A table directory's segments for callers that run the batch engine** (t-rust-db/tools#13). `engine::column::snapshot_segments` gives a `Snapshot`'s row groups and pages as `SnapshotSegment`s (one `Segment` each, in LSN order, with the table's NULL-fill and casts), keyed as the caller names the columns. `engine::column::table::read_snapshot` gives the whole table as one `Batch`, for a join's build side or a window's input. `engine::column::snapshot_files` opens the files the segments read. column-rs uses them to run joins and window functions over a table directory.

## [0.133.0] - 2026-10-16

### Added

- **`ingest-read` feature** (#574, ADR 0029). Reading a table directory (`Snapshot`, the column engine's table reader) no longer compiles the Parquet writer or the WAL writer: `ingest-read` is `storage-column` + `storage-stream` + `vm-batch`, and `ingest` is `ingest-read` + `storage-column-write`. `ColumnType`, `ColumnSpec` and `ColumnValues` live in `storage::parquet::schema` (re-exported from `parquet::writer`), `IngestError` in `storage::ingest::error` (re-exported from `ingest::table`).
- **`Table`** (#574). A cloneable handle over one `TableWriter` for embedding: `open`, `append`, `append_row`, `append_batch`, `sync`, `flush`, `compact`, `snapshot`, `close`, and `query` (with `engine-column`). It has no threads of its own; clones on other threads interleave call by call.
- **Line sources** (#574). `Table::ingest_lines` reads lines (stdin, a pipe) until end of input. `Table::follow` follows a log file through the WAL until the table is closed: it resumes where it stopped (`follow/*.offset`), reads a rename-rotated file to its end before switching, and starts again after truncation. Delivery is at least once.
- `examples/embed_table`: append, query, flush, query again.
- `TableWriter::sync`.

### Changed

- ADR 0029 is accepted, with an "As built" section. It departs from the proposal: two methods instead of an `Input` enum (`dyn` is outside the qualified subset), no ingest thread, and logfile mode goes through the WAL instead of using the file as the WAL.

## [0.132.0] - 2026-10-16

### Added

- **`staging/` spills for tiered tables** (#573). Past `TierOptions.spill_rows` sealed rows (default 1M), a tiered table writes its open hour to `staging/date=D/hour=H/` as a run sorted by the sort key, committed like a flush, so the rows leave memory and the WAL. The hour's close is a k-way merge of its runs and remaining pages into one file in `live/`, holding one row group per run rather than the hour; ties keep arrival order, and a lines table's runs merge under the widened schema. `Ack::Flushed` now works on a tiered table (it spills the open hour). Recovery cleans `staging/`.
- **Retention for tiered tables** (#573). `TierOptions.retention_days` keeps that many closed days in `archive/`; `compact()` drops older days whole in one commit and deletes their files after the grace period. The manifest records `dropped_lsn` (written only when not 0). DuckDB over `archive/` sees exactly the kept days.

### Changed

- `TierOptions` has two new fields, `spill_rows` and `retention_days`, and `Area` a new variant, `Staging`: code that builds `TierOptions` without `..Default::default()` or matches `Area` exhaustively needs updating. A manifest with a `staging` file or `dropped_lsn` is refused by db-core 0.131 and earlier.

## [0.131.0] - 2026-10-15

### Added

- **`MAP<UTF8, UTF8>` columns** (#621). The Parquet writer writes `ColumnType::MapUtf8` as DuckDB writes `MAP(VARCHAR, VARCHAR)`: a `key_value` group of a required UTF8 key and an optional UTF8 value, with repetition and definition levels and dictionary-encoded leaves. DuckDB 1.5.6 and pyarrow 25.0.1 read the maps back with their entries and entry order. `RowGroupReader::read_map_utf8_column` reads a MAP field, and nested leaves now take dictionary pages.
- **Maps in the column engine** (#621). A MAP field reads as a map value (`vm::batch::Value::Map`), printed as DuckDB prints a map (`{a=1, b=NULL}`). `m['k']` (column dialect) and `map_extract_value(m, k)` give the value or NULL; `element_at(m, k)` gives DuckDB's one-element list as text; `map_keys(m)` and `cardinality(m)` answer as in DuckDB; `map_get_ci(m, k)` looks a key up case-insensitively, for HTTP header names. Known scalar functions are now accepted anywhere in an expression, so a lookup can filter in WHERE.
- **MAP columns in record tables** (#621). A repeated key's values are joined at append, in order: with `", "`, or a newline for `Set-Cookie`. A MAP cannot be a sort key. ADR 0027 is amended.
- `examples/map_vs_flat` measures a MAP column against a flat key/value table: about the same bytes per request (63.8 vs 65.6) and lookup time.

### Changed

- `vm::batch::Value` has a new variant, `Map`. Code that matches `Value` exhaustively needs an arm (t-rust-db/tools#40 adds it to column-rs).

### Fixed

- **Columns after a nested field read the wrong chunk.** In a Parquet file with a group (MAP, LIST, STRUCT), the column engine and a tiered table's day merge numbered column chunks by schema element instead of by leaf, so every column after the group read another column's chunk: for DuckDB's `id, h MAP, x`, `SELECT x` failed and `SELECT key` read `x`'s integers as strings. `ParquetFile::fields` now lists each top-level field with the leaves it spans.

## [0.130.0] - 2026-10-15

### Added

- **Record tables** (#594). A table can be created with a fixed schema (`TableOptions::schema`) and appended to with typed rows (`TableWriter::append_row`) instead of log lines. A row must match the schema exactly: arity, NULL only in nullable columns, each value of its column's type and range. Nothing is converted. Flushes, hour and day merges write exactly the schema's columns, and a day merge reads each column back as its declared type, so logical types survive compaction. A record table's manifest is version 3; lines tables stay version 2.
- **Parquet writer types** (#595): BLOB (`ColumnType::Binary`), `UInt8/16/32/64`, `Decimal { precision, scale }` (INT32 up to 9 digits, INT64 up to 18, a 16-byte FIXED_LEN_BYTE_ARRAY above) and `FixedLenByteArray(n)`, with type-ordered statistics and both ConvertedType and LogicalType annotations. DuckDB 1.5.6 and pyarrow 25.0.1 read each type back with its declared type, values and bounds.
- **Column engine reads** of the new types, by annotation: unsigned values are unsigned (a `UINT_64` above `i64::MAX` is an error, not a wrapped negative), DECIMAL reads as a float, and BLOB and fixed-length bytes read as lowercase hex.

### Changed

- `ColumnValues::column_type()` is replaced by `ColumnValues::is_for()`: a decimal's or fixed-length column's parameters are not in its values.
- A BYTE_ARRAY without a UTF8 annotation reads as hex, not as a string (DuckDB's BLOB).

### Fixed

- **Pruning on unsigned statistics.** Footer statistics for `UINT_32` and `UINT_64` were read as signed, so a row group holding `u32::MAX` was skipped for `u32 > 3000000000`.
- **DECIMAL columns written by DuckDB** read as their unscaled integers; they now read as their value.
- **Ungrouped aggregates over no row groups.** `SELECT count(*) ...` returned no row when footer statistics pruned every row group, or when the file had none; it returns one row (count 0, the others NULL), as sqlite and DuckDB do.
- `make check-features`: the `vm-batch` gate on the `storage::column` decode re-export was lost, so `storage-column` alone failed to build.

## [0.129.2] - 2026-10-15

### Fixed

- **GROUP BY on a result-column position or alias** (#610). `GROUP BY 1` grouped on the constant 1 and folded every row into one group; it is now a 1-based position into the result columns, `*` expanded. A bare GROUP BY name is a FROM column first and a result-column alias only when no table has it, and HAVING sees aliases too, on one table and over a join. A position that is out of range, or a term naming an aggregate, gets sqlite3's error.
- **FROM subqueries without their own FROM, and `VALUES` in FROM** (#611). `SELECT x FROM (SELECT 1 AS x)` and `(VALUES (1,2),(3,4))` are materialized as constant rows, including through CTEs and views. `VALUES` columns are named `column1`..`columnN`; rows of different widths are refused.
- **Expressions in a joined aggregate** (#612). Result columns, GROUP BY keys, aggregate arguments, HAVING and ORDER BY terms of a joined GROUP BY or aggregate can be any expression, such as `upper(a.s)`, `COUNT(*) + 1` or `a.k % 2`. HAVING over a self-join no longer reads `b.s` as `a.s`, keeps a NOCASE column's collation, and refuses an ambiguous name as sqlite3 does.
- **ORDER BY a position over a join** sorted on the constant itself, leaving rows in join order (#612). It now sorts on that result column, under its collation.
- **Aggregates nested in CASE, function arguments or BETWEEN** (`coalesce(max(n), 0)`, `CASE WHEN count(*) > 1 ...`) now compile, with or without a JOIN (#612).
- **DISTINCT over a join honours column collations** (#613). A NOCASE column kept both `'a'` and `'A'`; inner, LEFT and FULL joins, sorted or not, now compare each column under its collation, through `*` and `table.*` too.

## [0.129.1] - 2026-10-14

### Fixed

- **logfmt and CLF tables store string values decoded** (#616), as #583 did for JSON. A quoted logfmt value decodes JSON's escapes, as go-logfmt reads it: `msg="a \"q\""` stores `a "q"`. A bare value such as `path=C:\new` keeps its backslash. CLF fields decode Apache and nginx escapes (`\"`, `\\`, `\n`, `\r`, `\t`, `\b`, `\v`, and `\xHH` read as bytes, then as UTF-8). Values read the same before and after a flush. `raw` stays as written, and the stream engine still returns values as written. ADR 0027 updated.
- **A CLF quoted field ended at an escaped quote** (#616). Apache writes `"` inside the request line as `\"`; the field was cut short there, and status, bytes, referer and user agent were read from the rest of the line. A quoted field now ends at the first `"` without a backslash before it, in tables and in the stream engine.

## [0.129.0] - 2026-10-13

### Changed

- **`Opcode::Combine` gains `output_order`** (#585): the select-list position of each merged column. Code that builds a `Combine` literal must set it (`Vec::new().into()` keeps the merged order). This breaks the public API, hence the minor bump.

### Fixed

- **Batch GROUP BY returns columns in select-list order** (#585). Grouped rows were merged keys-first, so in `SELECT MIN(n), n … GROUP BY n` each header labelled the other column's values. This affected `BatchEngine`, the table engine and the stream engine. ORDER BY on a key listed after an aggregate also sorted by the wrong column.
- **A grouped aggregate expression returned NULL** for every group (`SELECT k, SUM(x) * 2 … GROUP BY k`): `Combine` read the expression's row count from the key column it had already moved out.
- **JSON tables store string values decoded** (#583). `{"msg":"a\"b"}` stored `a\"b`; it now stores `a"b`, as DuckDB's `read_json` reads it. Values are decoded at flush, at hour close, and when a query reads an unflushed page, so flush timing never changes an answer. `raw` stays as written, and a line that is not a JSON object keeps its whole text as `message`. A lone surrogate becomes U+FFFD; any other malformed escape is kept as written. `make check-parquet-write-oracle` compares stored fields with DuckDB's `json_extract_string` of each `raw` line. ADR 0027 updated; logfmt and CLF quoting is tracked in #616.
- **Flaky `run_morsels` rebalancing test** (#580): it asserted wall-clock time and failed under load. It now checks that the other workers drain the queue while one morsel is blocked.

## [0.128.2] - 2026-10-13

### Fixed

- **Alias-qualified columns in the row engine** (#604). `SELECT a.x FROM t AS a` and columns of an aliased FROM subquery resolved against the table name, not the alias, and were refused.
- **`rowid`, `oid` and `_rowid_`** (#605) are readable on any rowid table, also when no `INTEGER PRIMARY KEY` aliases them, in plain, sorted and grouped scans. A declared column with one of these names shadows the rowid, as in sqlite3. Silent wrong results fixed along the way: an expression over the `INTEGER PRIMARY KEY` after ORDER BY (`id+1`) and `MAX(id)` under GROUP BY read the wrong value.
- **FROM subquery without an alias** (#606) is accepted; its columns are named under `(subquery)`.
- **`GROUP BY k ORDER BY k` sorts once** (#607). When ORDER BY lists the group keys in order, the grouping sort takes its directions and the post-group sort is skipped, as sqlite3 does; EXPLAIN QUERY PLAN no longer shows the extra temp b-tree.
- **DISTINCT over a joined aggregate** (#608) was refused; it now dedups each group's row before ORDER BY / LIMIT, under each column's collation. EXPLAIN QUERY PLAN reports the GROUP BY, DISTINCT and ORDER BY temp b-trees for joined aggregates.

### Security

- **`ruzstd` 0.7.3 → 0.9.1** for RUSTSEC-2026-0344 (a small `.zst` input could expand one block to gigabytes). ZSTD page decoding now also reads at most one byte past the page's declared uncompressed size.

### Known gaps

GROUP BY on a result alias is refused (#610); a FROM subquery without its own FROM, `(SELECT 1 AS x)`, is refused (#611, pinned in `tests/unit/sqlite_parity_test.rs`); joined-aggregate result columns must be plain columns or aggregates (#612); plain-join DISTINCT ignores collations (#613).

## [0.128.1] - 2026-10-13

### Fixed

- **Row engine result column names follow sqlite3** (#596). An unaliased expression is named by its source text as written; a column by its declared name; `rowid` by the `INTEGER PRIMARY KEY` column; `*` and `t.*` by the sources' names, with `USING`/`NATURAL` columns listed once. Derived tables repeat names as `name:1`, `name:2`. Previously `column1`, `column2`, and so on.
- **Quoted column names with spaces** (`"x y"`) were read from the catalog as `x` with a garbled type.
- **`WITH ... SELECT` returned no column names**: it ran as a plain statement.
- **ORDER BY and DISTINCT over an aggregate in the row engine** (#597). `GROUP BY ... ORDER BY`, ORDER BY on an aggregate without GROUP BY, and DISTINCT over an aggregate were refused. Dedup compares under each column's collation; an ORDER BY term naming no output column becomes a hidden column. EXPLAIN QUERY PLAN reports the temp b-trees as sqlite3 does.
- **`INSERT ... SELECT ... GROUP BY` with more than one group** inserted wrong rows: the per-group flush shared a fixed cursor with the INSERT's scan.

### Known gaps

Pinned in `tests/unit/sqlite_parity_test.rs` (each asserted to still fail until fixed): #604 alias-qualified columns, #605 `rowid`/`oid`/`_rowid_`, #606 unaliased FROM subqueries. Also #607 (an extra sort for `GROUP BY k ORDER BY k`) and #608 (DISTINCT over a joined aggregate).

## [0.128.0] - 2026-10-12

### Added

- **Tiered layout for ingested tables** (#573 part (a), ADR 0028 accepted with an as-built amendment). The layout is a per-table choice, fixed at creation: `TableOptions.layout` is `Layout::Flat` (one file per flush in `data/`, the default and unchanged) or `Layout::Tiered(TierOptions)`.
  - **Hour close:** a tiered table partitions by arrival time, and the head seals at every hour boundary. `flush()` writes each closed hour as one file in `live/date=D/hour=H/`, rows sorted by `sort_key` across the whole hour, then cut into row groups of at most `row_group_max_rows`. So each row group covers a range of the key and footer statistics skip row groups on it.
  - **Day merge:** `TableWriter::compact()` flushes closed hours, then merges each closed day's hours into `archive/date=D/`, starting a new file at an hour boundary past `max_file_size`. Every output is built under `tmp/`, published with one directory rename, and committed with one manifest swap.
  - **Deferred deletes:** replaced hour files are deleted after `delete_grace_ns`, for readers that list directories.
  - **Clock:** `TableOptions.clock` sets arrival time; the system clock by default.
- **Manifest version 2:** records `layout` and each file's `area`. Version-1 manifests read as flat tables.
- **Readers:** `Snapshot::open` re-reads the manifest when a listed file has vanished (a compaction replaced it). Recovery removes unlisted files and empty directories in `data/`, `live/` and `archive/`.
- **Tests:**
  - an hour/day/archive walk-through
  - a crash after every flush and compaction step recovers every record once
  - files split at the size limit
  - recovery of strays in every area
  - a sort key letting statistics skip 2 of 3 row groups (0 in arrival order)
  - the flush-timing property test runs half its seeds tiered, with a moving clock and random compactions
  - `make check-parquet-write-oracle` has DuckDB read `archive/*/*.parquet` and get exactly the closed days' rows

### Changed

- **Breaking:** `TableWriter::flush()` and `flush_all()` return `Vec<FileEntry>` (a tiered flush can close several hours). `Manifest::new` takes the `LayoutKind`. `FileEntry` gains `area`, and `IngestError` gains `LayoutMismatch` and `Unsupported`. `Ack::Flushed` is refused on a tiered table until `staging/` spills exist (#573 part (b)).
- `ParquetWriter::bytes_written()` and `set_key_value_metadata()` are new.

## [0.127.0] - 2026-10-11

### Fixed

- **A field whose type changes inside one page no longer loses values** (#584; ADR 0018 amended). `FieldColumn` used to degrade a conflicting column to `Str`, which turned every number and boolean into NULL: `{"s":"v1"}` then `{"s":3}` lost the 3, and `{"z":2}` then `{"z":2.5}` lost both. Stream queries never saw those values, and ingested tables (#572) wrote the NULLs to Parquet.
  - **Integer and float:** an integer column meeting a float now widens to `Float`, as a flush widens to DOUBLE.
  - **Any other conflict:** the column becomes the new `FieldColumn::Mixed` / `OwnedColumn::Mixed`, which keeps each value as parsed (`FieldValue` / `MixedValue`).
  - **How it reads:** queries read a mixed column as strings, each typed value rendered the way a flush renders it into a string column (`3`, `2.5`, `true`). So a record reads the same before and after its flush.

### Changed

- **Public API:** `FieldColumn` and `OwnedColumn` gain a `Mixed` variant, and the enums `FieldValue` and `MixedValue` are new. Code that matches every variant must handle `Mixed`.
- **Tests:** the flush-timing property test now mixes types inside pages, including a field that takes all four types at random.

## [0.126.0] - 2026-10-11

### Added

- **Snapshot reads of ingested tables** (#572 part (b), closing #572; ADR 0027, amended). `Snapshot` is one fixed view of a table: the manifest's files (memory-mapped) plus the records above `flushed_lsn` as pages, every LSN exactly once.
  - `TableWriter::snapshot()` gives an in-process view up to the last append.
  - `Snapshot::open(dir)` gives the view from another process, with no lock and nothing written. It reads the manifest, then the WAL above `flushed_lsn` read-only (new `recover::read_tail`). A partial last frame ends the read; if a flush moved on in between, it reads both again.
- **`engine::column::table`**: `query_snapshot(snapshot, table, sql)` and `TableEngine` (`Mode::Batch`, opened on the table directory, a fresh snapshot per query). The table's columns are the union of every file's and page's columns, widened by the flush rule.
  - A column a segment lacks reads as NULL; one stored with another type is cast (`snapshot::cast`), so a record reads the same before and after its flush.
  - Row groups keep footer pruning, top-N bounds and late materialization, through columns stored with the table's type only.
- **Tests**:
  - a property test that query results do not depend on where seals and flushes fell, in and out of process
  - NULL-fill and widening across files and pages, including pruning that would be wrong on a widened column
  - snapshots across an interrupted flush at every step
  - read-only WAL tail cases
  - `tests/table_reader.rs` with `tests/helpers/table_probe.rs`: a reader in another process, while a writer appends, seals and flushes, always reaches the writer's last durable ack and holds every LSN once
  - Found while testing, filed separately: #584 (the stream parser loses field values when one page mixes types, and flush stores those NULLs) and #585 (batch `GROUP BY` returns group keys first, under the wrong column names, also in `BatchEngine`).

## [0.125.0] - 2026-10-11

### Added

- **Flush: ingested records become Parquet files under a manifest** (#572 part (a), third story of the write-path epic #570; ADR 0027). `TableWriter::open(dir, TableOptions)` locks the table, reads `manifest.json`, cleans up (empties `tmp/`, deletes files in `data/` the manifest does not list) and replays the WAL into pages. `append(line, Ack)` logs the record (WAL payload: `observed_ts` i64 LE plus the line) and adds it to the head page; a full head (`SEGMENT_MAX_ROWS`) seals itself, `seal_head()` seals it on demand. `flush()` writes the sealed pages to one file in five steps: write and fsync `tmp/<lsn_lo:020>-<lsn_hi:020>.parquet`, rename into `data/` and fsync the directory, replace `manifest.json` atomically (tmp, fsync, rename, directory fsync; the single commit point), drop the pages, unlink WAL segments at or below `flushed_lsn`. `Ack::Flushed` returns after the commit.
- **Manifest** (`manifest.json`): the table's format, `flushed_lsn` and the file list with per-file LSN range, rows, bytes and `observed_ts`/`timestamp` bounds. The invariant (files contiguous from LSN 1, one row per LSN, last `lsn_hi == flushed_lsn`) is checked on every read and write.
- **File layout**: the seven predefined stream columns (`timestamp`, `observed_ts` as nanosecond timestamps, `severity` INT32, `severity_text`, `facility`, `message`, `raw`), then the union of every page's fields in first-seen order. Integer/float conflicts widen to DOUBLE, any other conflict to a string. Each file's key-value metadata records `db-core.lsn_lo`/`db-core.lsn_hi`.
- **Parquet writer**: `ColumnType::TimestampNanos` (written as DuckDB writes `TIMESTAMP_NS`: `isAdjustedToUTC=false`, no ConvertedType) and `WriterOptions::key_value_metadata`.
- **`Segment::seal_lines`**: a segment with one row per line and a per-row `observed_ts`.
- **ADR 0027 "Where tables differ from DuckDB"**: timestamp marking and range, file-only readers miss unflushed rows and a glob over `data/` can catch an uncommitted file (read the manifest's list), files need `union_by_name`, type-conflict widening, non-JSON lines kept as rows, one-level nesting flatten, arrays as JSON text, string values keep their JSON escapes (ADR 0018), reserved predefined names, `severity` as INT32 plus `severity_text`.
- **Tests**: a crash after each flush step recovers every record exactly once and the table carries on; reopen restores pages with their arrival times; manifest round trip, invariant and malformed-input cases; the write oracle now also has DuckDB and pyarrow read a flushed table.

## [0.124.0] - 2026-10-11

### Added

- **Record WAL: `storage::ingest`, feature `ingest`** (#571, second story of the write-path epic #570; ADR 0027, 0029). `Wal::open(table, flushed_lsn, options)` locks the table, replays `wal/` and starts the WAL thread. `append(record, Ack)` assigns the next LSN and frames `[len u32][crc32c u32][lsn u64][payload]` into a shared buffer; the WAL thread group-commits (one write, one fsync per round), advances `durable` and wakes waiters. `Ack::{Memory, Durable, Flushed}`, and `flushed <= durable <= visible` always holds. Segment files `wal/<lsn_lo:020>.wal` are created on first write (directory fsynced) and closed at `roll()` (exact frame boundary) or the size limit (64 MiB). `mark_flushed` unlinks the segments wholly at or below the flushed LSN. CRC-32C is hand-rolled; there is no new dependency.
- **Recovery** replays from `flushed_lsn` and requires consecutive LSNs. It truncates only a torn tail: a bad frame in the last file with nothing valid after it. Any other damage (a bad frame in an earlier file, a bad frame followed by valid ones, an LSN gap, a missing file) fails with `WalError::Corrupt` and leaves the files untouched, rather than drop records that may have been acknowledged. ADR 0027 is amended accordingly.
- **`TableLock`**: one writer per table through `<table>/LOCK`. Acquiring retries for about 50 ms, because a child forked by another thread shares the lock until it execs.
- **Logfile mode**: `LogFile::open_at(path, offset)` resumes at a byte offset (the LSN in that mode), and `FileIdentity`/`identity()`/`rotated()` detect a rename-and-recreate rotation.
- **Tests**:
  - every truncation of a valid log replays its complete prefix
  - bit flips are detected
  - group commit: 1,600 appends from 8 threads take far fewer fsyncs
  - ack levels, roll and unlink, lock exclusion, I/O failure
  - `tests/wal_crash.rs`: SIGKILL torture that also simulates power loss by cutting the unacked tail; 300 kills with 147 cuts lost no durable ack

## [0.123.0] - 2026-10-11

### Added

- **Parquet writer for flat schemas: `storage::parquet::writer`, feature `storage-column-write`** (#566, first story of the write-path epic #570; ADR 0029). `ParquetWriter` takes column batches (`ColumnValues`: boolean, int32, int64, float, double, UTF-8 string, UTC timestamp in microseconds; `required` or `optional`) and writes a row group per `row_group_rows`, then the footer on `finish()`. Each column chunk is one DATA_PAGE v1, preceded by a PLAIN dictionary page when at most half the values are distinct and the dictionary fits `dictionary_max_bytes`; definition levels and dictionary indices use the RLE/bit-packed hybrid. Pages are uncompressed or Snappy, with a hand-rolled compressor, so there is no new runtime dependency. Every chunk carries `min_value`/`max_value`/`null_count` in the type-defined orders (NaN skipped, zero bounds widened to -0.0/+0.0), and the footer declares a `TypeDefinedOrder` per column. The metadata goes through a Thrift compact encoder over the reader's `Value` tree. All writer code is under the full lint bar and in the qualified subset, with no exemption.
- **Tests:**
  - unit tests per module, plus seeded random round trips of each encoder through the reader's decoders
  - `tests/parquet_writer.rs`: write, then `ParquetFile::open`, comparing values and checking that every chunk's statistics bound its values, including a proptest over random schemas, data, codecs and row-group sizes (`proptest` is a new dev-dependency)
  - `make check-parquet-write-oracle`, a new CI job: DuckDB 1.5.6 and pyarrow 25.0.1, pinned and run through uv, read the files back and agree on types, every value and per-row-group statistics; neither enters the crate's dependency closure

### Changed

- **The Parquet reader moved from `storage::column::parquet` to `storage::parquet`** (#566), next to the writer. The old path keeps working: `storage::column` re-exports the `parquet` module, as well as `ParquetFile`, `FileError`, `RowGroupReader` and `DictionaryIndices`. The reader's lint allowances (`arithmetic_side_effects`, `indexing_slicing`, `missing_docs`) now apply per reader module, not to everything under `parquet`, and `MVL_LIMIT_EXCLUDE` names the reader files instead of `parquet/*`, so new files there are gated by default. The reader's lint burn-down is #578.

## [0.122.0] - 2026-09-22

### Added

- **`make fuzz-diff`: differential fuzzing against the pinned sqlite3 oracle** (#546, PR #560; Phase 1b of epic #543): every VM-reaching statement also runs on a persistent `sqlite3` shell per lane (own fixture copy, `.mode quote`, sentinel-delimited, version asserted against 3.53.4 via `ORACLE_BIN=`) and the two outcomes are joined into a verdict: pass, wrong-answer, float-drift (reals within 1e-12 relative), gap (we reject, oracle runs), unsupported (counted only), over-permissive (we run, oracle rejects), both-reject, oracle-hang. Rows compare sorted without `ORDER BY`, by count under `LIMIT` without `ORDER BY`; reals on both sides normalize through `format_real`. When exactly one side applies a state-changing statement both sides reset to the fixture (a new epoch), since replay cannot resync them. Findings dedupe by `(class, leading keywords, normalized message)`, a statement-level ddmin reducer rewrites one `.sql` repro per group, and `report.md` summarizes the run. Exit 4 for differential findings only. First 2000-statement run filed #561 (`PRAGMA journal_mode` returns no row), #562 (DDL sqlite3 rejects), #563 (`INSERT ... (cols) DEFAULT VALUES`, `PRAGMA synchronous` in a transaction), #564 (`EXPLAIN` over non-SELECT).

## [0.121.0] - 2026-09-21

### Added

- **`make fuzz-sql JOBS=n` parallel lanes** (#543, PR #559): each lane is a `Runner` with its own fixture copy and engine; statements are dealt round-robin by generator index so `(seed, index)` still names a statement and `REPLAY` keeps working. Findings carry `lane`/`jobs`. Temp fixture names gain a process-wide counter (lanes spawned within the same nanosecond collided). 20k statements: 33 s -> 9 s at `JOBS=8`. With this, `N=100000` over seeds 1–4 ran clean (0 panics/hangs/corruption), completing epic #543's Phase 1a.

## [0.120.0] - 2026-09-21

### Changed

- **`make fuzz-sql` report counts rejections in the headline and findings per class** (#543, PR #558): `statements / ok / rejected / skipped`, then the per-stage rejection table, then `findings / panic / hang / corruption`, so panics are counted on their own line. `RunSummary::rejected_total()` and `findings_of(class)` back the new lines.

## [0.119.2] - 2026-09-21

### Fixed

- **`DROP INDEX IF EXISTS` on an unknown name is a no-op** (PR #557) instead of `no such index`, matching sqlite3 and the `DROP TABLE IF EXISTS` fix in 0.119.1. Surfaced by reading `make fuzz-sql VERBOSE=1` rejection messages.

## [0.119.1] - 2026-09-21

### Fixed

- **Tables and views share one namespace; `DROP TABLE` refuses views** (#551, PR #556): `CREATE TABLE`/`CREATE VIEW` now check both catalogs and report the existing object's kind as sqlite3 does (`table t already exists` / `view v already exists`), `IF NOT EXISTS` is a no-op across both kinds, `DROP TABLE` on a view says `use DROP VIEW to delete view v` (also with `IF EXISTS`), and `DROP TABLE IF EXISTS` on an unknown name is a no-op instead of `no such table`. Before, a view created over an existing table made the second `DROP TABLE` free a root page another catalog row still owned (`2nd reference to page 2`), and once `sqlite_master` spilled onto that page the catalog became unreadable. Found by `make fuzz-sql`: 16 corruption findings in 5000 statements at seed 1, 0 after. New `DispatchError::{ViewAlreadyExists, UseDropView}`.

## [0.119.0] - 2026-09-21

### Added

- **Honest grammar coverage in `make fuzz-sql`, plus `UNEXERCISED=1`** (#543, PR #555): the coverage denominator counts only alternatives reachable from the entry rule (SHARED rules the row section never references, such as `comparison-op`, no longer count) and skips rules the dialect hook answered for. `Walker::unexercised()` lists what remains as EBNF-ish text; the run binary prints it under `UNEXERCISED=1` or `VERBOSE=1`. 117/117 with the dialect at 20k statements, 128/128 generation-only.

## [0.118.0] - 2026-09-21

### Added

- **`make fuzz-sql STAGE=parse|codegen|vm`** (#543, PR #554): stops the totality probe chain after the requested stage, for parser-only or codegen-only fuzz runs. A statement that clears the last requested stage counts as ok; the `quick_check` health probe only runs when the VM stage is in the chain. Default `vm` is unchanged.

## [0.117.0] - 2026-09-21

### Added

- **`make fuzz-sql`: per-stage totality runner for the ROW frontend (`fuzz-run` crate)** (#545, PR #553; second increment of fuzzing epic #543): `fuzz-gen` gains a `Dialect` substitution hook so `table-name`/`column-name`/`identifier`/`STRING`/`NUMBER`/`BLOB`/`type-name` are drawn from the opened fixture's live catalog (`tests/fixtures/btrees/select_parity.db`) and typed literal pools instead of grammar prose. The new `fuzz/run` workspace member runs every generated statement through three probes in order -- `parser::row`, then `codegen::row` compile-only via `Engine::explain_opcodes`, then `vm::row` -- each under `catch_unwind` in a worker thread with a per-statement timeout, so a panic names the layer at fault. Typed rejections are counted per stage; findings are panic, hang, or corruption (catalog unreadable or `PRAGMA quick_check` not `ok` after a VM-reaching statement), the latter carrying the engine's full statement history as a self-contained `.sql` repro. `make fuzz-sql N= SEED= MAX_DEPTH= TIMEOUT_MS= OUT= REPLAY=seed:idx` exits 3 on findings; `make fuzz-gen` keeps the generation-only view. First runs filed #551 (view/table name collisions corrupt the file on `DROP TABLE`) and #552 (`DROP VIEW` rejected at codegen).

### Fixed

- **db-core builds warning- and clippy-clean with `engine-row` alone**: `RowEngine::table_schema`/`with_storage` are gated on `vm-batch` (their only non-test consumer is `engine::cross_mode`), and `QueryResult` literals spell the `vm-stream`-only field under `cfg` instead of `..Default::default()`.

## [0.116.0] - 2026-09-21

### Added

- **Grammar-driven SQL statement generator (`fuzz-gen` crate, `make fuzz-sql`)** (#544, PR #550; first increment of fuzzing epic #543): the root `Cargo.toml` is now a workspace (`members = [".", "fuzz/gen"]`) with a shared `[workspace.lints]` table so `fuzz/gen` inherits the same strict lint bar. The new `fuzz-gen` crate holds a recursive-descent EBNF parser for the three sections of `src/parser/grammar.ebnf` (SHARED / COLUMN-RS / SQLITE-RS), tagging `(* Vn *)` V-blocks per alternative, and a seeded, depth-bounded, weighted-alternative `Walker` that produces statement strings for a chosen entry rule with `VBlockScope` filtering and a rule-coverage report. Termination is backed by a fixpoint minimum-height computation over the grammar, so an exhausted depth budget returns a typed `WalkError` rather than overflowing the stack. `make fuzz-sql TARGET=row|column|stream N= SEED= MAX_DEPTH=` generates statements only; the totality runner (`catch_unwind`, per-statement timeout) follows in #545 and the sqlite3 oracle comparison in #546.

## [0.115.1] - 2026-09-20

### Changed

- **`UPDATE` rewrites a row whose rowid is unchanged with one `Opcode::Update` instead of `Delete`+`Insert`** (#524, PR #541): the two-op pair each independently walked root-to-leaf (`find_leaf_page`) and did its own pager dirty-map bookkeeping for what is provably the same leaf page both times. New `btree::update_row` does a single descent, splicing the old cell out and the new one in on the same page (falling back to the two-descent path only when the leaf would otherwise collapse). A/B on `update_filtered_range` (1MB fixture) showed no measurable improvement over the pre-existing ~6.5x-vs-sqlite3 ratio -- within bench noise, same pattern as #533 -- so this is a correctness-neutral cleanup, not a fix for #524's gap, which stays open.

## [0.115.0] - 2026-09-20

### Added

- **`Opcode::Filter`/`Opcode::FilterAdd` bloom-filter opcodes for ephemeral-index probes** (#527, PR #540): mirrors sqlite3's own `Filter`/`FilterAdd` pair (confirmed present in its `EXPLAIN` for `IN (SELECT ...)`), which db-core previously had no equivalent for. Backed by a 64 KiB two-hash bloom filter on `EphemeralIndexCursor`, exposed via new `Cursor::filter_add`/`filter_maybe_present` trait methods (safe no-op defaults for every other cursor kind). Wired into every ephemeral-index `Found`/`IdxInsert` dedup site (`IN (subquery)` build+probe, `SELECT DISTINCT`, `LIMIT`-scan, DISTINCT+JOIN, `count(DISTINCT x)`) except `join_full.rs`'s FULL/RIGHT JOIN unmatched-row detection, which reuses the same opcodes with inverted semantics and is left for a separate, correctness-reviewed change. `EXPLAIN x IN (SELECT ...)` now shows `Filter`/`FilterAdd` in the same position sqlite3's own plan does. Measured impact is real but modest: `in_subquery_agg_outer` moved 6.2x -> 5.3x sqlite3 at 1MB, 4.7x -> 4.2x at 50MB -- #525's <=3x ratio target is not yet met; the remaining gap is elsewhere in the per-row VM/adapter cost.

## [0.114.8] - 2026-09-20

### Changed

- **`TableCursorAdapter` reuses its header-entries `Vec` across rows** (#533, PR #537): `ensure_cached()` replaced its cached `(Payload, Vec)` tuple wholesale on every row, dropping and reallocating the header-entries buffer each time even though `parse_header_into` already supports clear-and-refill. The cache is now split into a `payload` field (reset per row) and a persistent `entries` field whose backing allocation survives across rows. A/B benchmark against the sqlite3 oracle (`full_scan`/`full_scan_3col`, 1MB and 50MB fixtures) showed no measurable improvement -- deltas were within the ~10% bench-noise floor -- so this is a correctness-neutral allocation-hygiene cleanup, not a fix for #533's ~2-3x full_scan gap, which stays open.

## [0.114.7] - 2026-09-19

### Changed

- **`EphemeralIndexCursor` (`IN (SELECT ...)`/`DISTINCT` probes) switched from a `BTreeMap` to a `HashMap`, plus a typed `i64` fast path** (#527, PR #529): the ordered-bound guarantee a `BTreeMap` gives was never used by this cursor kind (`IdxLE`/`IdxCompareGT` are only ever compiled against a real index cursor opened via `OpenRead`, never an `OpenEphemeral` slot), so the cursor now gets O(1) average membership instead of an O(log n) descent. A single-column `INTEGER` key -- the common `IN (SELECT int_col ...)`/`DISTINCT int_col` shape -- additionally skips `normalize_key_values`/`encode_record` entirely and goes straight into a dedicated `int_entries: HashMap<i64, Vec<Value>>`. A/B/A on the `vm_opcodes/row::Found+AggStep` micro-benchmark (#526): ~540-560ns/call (`BTreeMap`) -> ~245-260ns/call (`HashMap` + fast path), a ~2.1x improvement.

## [0.114.6] - 2026-09-19

### Changed

- **`UPDATE` skips index maintenance for indexes the `SET` clause doesn't touch** (#524, PR #528): every index on the table was rebuilt (`IdxDelete`+`IdxInsert`) per matched row regardless of whether `SET` assigned any of that index's columns, making `update_filtered_range` 12.7x slower than sqlite3 (8.7us vs 0.7us per row). `emit_index_key_ops`/`emit_index_key_ops_from_regs` now take a per-index `touched` flag computed from the assignment list (or forced for every index when the rowid is reassigned, since it's part of every index's key); `INSERT`/`DELETE` are unaffected since a full row write/removal legitimately invalidates every index.

### Added

- **`EXPLAIN` accepts an `UPDATE`/`DELETE` body** (#524, PR #528): `ast::Explain::select: Box<Select>` is replaced by `ast::Explain::body: ExplainBody` (`Select`/`Update`/`Delete`), needed to diagnose per-row index-maintenance cost like the above from the CLI. `EXPLAIN QUERY PLAN` over a non-`Select` body remains `Unsupported` (EQP is join-planner output with no equivalent for a write statement).

## [0.114.5] - 2026-09-19

### Changed

- **Dropped the per-row has-seen-a-row flag around implicit-group `AggStep`** (#525, PR #526): `try_compile_direct_agg_scan`'s whole-table-group scan only runs once per program execution, so an agg-context slot always starts `None` -- `AggState::initial`/`finalize(None)` already give correct zero-row semantics without a forced reset. Every row now simply folds, and the `have_group_reg` boundary branch (`Eq`/`Goto`/`Integer`/`Goto`, 5 opcodes/row) is only emitted when the select list needs a first-match snapshot of a plain column; a pure `count(*)`/aggregate-only projection -- the `IN (subquery) ... COUNT(*)` shape #525 reports at 8-10x sqlite3 -- drops the branch entirely.

## [0.114.4] - 2026-09-19

### Changed

- **`col < ?` / `col <= ?` (and `? > col` / `? >= col`) seek the index as an upper-bound-only range** (#508, PR #523): the row planner recognised only lower-bound comparisons, so `a <= 20` compiled to a full scan whatever the statistics said. `as_forward_comparison` now accepts all eight spellings and reports which side the bound is on; upper-bound shapes compile to `IdxRewind` then a walk stopped by `IdxCompareGT` (`<=`) or the new `IdxCompareGE` opcode (`<`), shared by the SELECT emitter and the UPDATE/DELETE + aggregate row seek. EQP reports `SEARCH t USING INDEX ia (a<?)`; the #498 stat4 gate receives the bound as an upper bound, so `a <= 20` seeks and `a < 4000` scans on the stat4 fixture, both as sqlite3 3.53.4 plans them. Also fixes `? < col` / `? <= col` being estimated as an upper bound while compiled as a lower-bound seek. Parity 19/19.

## [0.114.3] - 2026-09-19

### Changed

- **Record header vectors presized; sorter parses the key header once** (#486, PR #520): a profile of sqlite-rs's `group_by_agg` (50 MB) put ~10% of VM time in `Vec::grow_one` under the storage `decode_record`'s serial-type vector, and `SorterCursor` re-parsed the record header once per sort-key column. Both header vectors are now sized from the header's byte length (clamped to the payload) and all sort keys decode off one parsed header. A/B/A `ours`: 1 MB 8.76 -> 7.89 ms, 50 MB 456 -> 423 ms.
- **`HashAggCursor` groups are indexed by a `compare`-consistent key hash** (#486, PR #520): the O(groups) linear scan in `find_group` is replaced by a `HashMap<u64, Vec<usize>>` bucket index whose hash normalises exactly the equalities `compare` reports (integral REAL as its INTEGER, NaNs canonical, NOCASE lowercased, RTRIM trimmed); equality still goes through `keys_equal`. `HashAggStep` borrows its argument registers as a slice instead of cloning them into a per-row `Vec`. Codegen does not emit this cursor yet (#519), so no benchmark moves on its own.

## [0.114.2] - 2026-09-19

### Changed

- **`prepare` built the projection's expression scope once per result column** (#486, PR #521): `compile_row_values` called `Scope::single_shared(..).with_catalog(catalog)` for every result-column expression, and `with_catalog` copies every `TableSchema` into a fresh `Rc<[TableSchema]>` that was dropped right after -- ~60% of prepare time for a plain five-column `SELECT` in sqlite-rs's `prepare_only` bench. The scope is now built lazily once per projection: 6.63 -> 3.68 us (sqlite3 1.05 us), 6.4x -> 3.5x.

## [0.114.1] - 2026-09-19

### Changed

- **`TableCursorAdapter::column()` decoded a row's entire record per requested column** (#485, PR #518): the dormant per-row cache field is now wired up -- the payload and its parsed header (serial type + body offset per column) are cached once per positioned row via `parse_header_into`, and `column()` decodes only the requested column's body from that cache. `SorterCursor::column()` had the same full-decode-per-call pattern; switched to `decode_column()`.

## [0.114.0] - 2026-09-19

### Changed

- **Live top-N threshold skips row groups by footer statistics** (#464, PR #512): `Segment` gains `order_key_bound(descending)` -- the best `ORDER BY` key a segment could produce from its summary without loading (`None` = unknown, always loaded). `run_parallel_top_n` claims segments best-bound-first and feeds one shared bounded heap, so a segment whose bound cannot beat the current n-th candidate is never loaded; `engine::column::RowGroupSegment` answers from the row group's `min`/`max` for a single bare-column `ORDER BY` via the same `prune::decode_stat_bytes` #458's WHERE pruning uses. Parity `large.parquet` `ORDER BY amount DESC LIMIT 100`: 82 -> 56-58 row groups loaded, 21.5 -> 15.3 ms in-process.
- **Non-key top-N columns are late-materialized; PLAIN pages gathered positionally** (#513, PR #515): `Segment` gains `load_columns(names)` and `gather_at(names, positions)`. For a plain `ORDER BY ... LIMIT` body, `top_n_over_segments` loads only the key (and predicate) columns, heaps, then gathers the other output columns at the winning positions only. `storage::column` gets a truly positional PLAIN gather (`read_plain_fixed_at`, `read_plain_fixed_column_at`: decompress only the pages holding a wanted row, stop after the last), which `decode_column_at` uses for its fixed-width arms -- also speeding ADR-0026's filtered projections. In-process on `large.parquet`: `SELECT id, amount ... ORDER BY amount DESC LIMIT 100` 14.8 ms / 179 MB -> 11.4 ms / 137 MB; four output columns 41.4 -> 30.1 ms.
- **Join probe stops SipHash-ing an already-computed hash; the fused fold groups by build row** (#514, PR #516): `BuildTable::index` re-hashed every probe row's `u64` with SipHash and verified through a `Value == Value` compare; the slot is now `murmur_finalize(hash) & mask` with a typed key compare. For an inner join grouped only by payload columns, `fold_group_row` no longer hashes the payload string per matched row: `BuildTable` classifies its rows once (`payload_groups`, a `OnceLock` shared by all probe workers) and the fold indexes by build row. Fused `HashProbeGroupReduce` per fact row 58 -> 28 ns; `run_join_segments` on the parity join, 12 threads, 100 -> 54 ms wall.

## [0.113.2] - 2026-09-18

### Changed

- **`primary_expr`'s `Identifier` arm double-cloned a column reference's identifier `String`** (#486): once via the tokenizer/`identifier()`, again via `parts.as_slice()`'s slice pattern forcing a clone to build `ExprKind::Column`. Moved out of `parts` by value instead. ~7% faster parse for a 5-column projection; most of #486's `prepare_only` 6.3x gap against sqlite3 is elsewhere (`primary_expr`'s own whole-token clone, not fixed here).

## [0.113.1] - 2026-09-18

### Fixed

- **A failed `CHECK` is reported by its constraint, not by the table** (#503): `INSERT`/`UPDATE` said `CHECK constraint failed: t`; sqlite3 3.53.4 names the declared `CONSTRAINT name`, or, for an unnamed check, the verbatim source text between the parentheses -- `CHECK( a  >  0 )` reports `a  >  0`. The grammar now keeps the `CONSTRAINT` identifier for `CHECK` and records the span of the parenthesized body (`ColumnConstraint::Check`/`TableConstraint::Check` gained `name` and `body`); `codegen::row::stmt::insert::check_label` slices that span out of the table's stored DDL at compile time, shared by the INSERT and UPDATE emitters. Six shapes (named/unnamed, column/table level, inner whitespace, nested parentheses, UPDATE) verified identical to the oracle.

## [0.113.0] - 2026-09-18

### Added

- **`sqlite_stat4` histograms decide seek versus scan for a range predicate, exactly as sqlite3 does** (#498, item 3 of #484 re-scoped): `engine::row::stats::load_stats` now also reads `sqlite_stat4` (per-index samples with their `neq`/`nlt`/`ndlt` counts), and `codegen::row::planner` carries a port of sqlite3's cost model in its own `LogEst` units -- `whereKeyStats`/`whereRangeScanEst` (sample bisection with the 1/3 and 2/3 gap rounding, the same-gap quarter rule, the fixed 1/4 and 1/64 defaults for a bound the histogram cannot see), `estimateTableWidth`/`estimateIndexWidth` from declared types (`sz=` hints honoured), and the `whereLoopAddBtree`/`whereLoopAddBtreeIndex` costs with sqlite3's tie rule. `range_seek_beats_scan` gates every `BETWEEN`/forward-comparison range seek in `range_scan.rs` -- direct scan, aggregate and EXPLAIN QUERY PLAN in lockstep -- so on a file real sqlite3 has analyzed, a wide closed range now plans as `SCAN` where the oracle does (`sum(f) WHERE x BETWEEN 1000 AND 60000` on the 50 MB bench fixture: 727 ms as an index seek, 325 ms as the scan sqlite3 also picks) and a narrow or single-sided one still seeks. Reproduces all 14 plan choices probed against sqlite3 3.53.4 on two analyzed files, including the one-unit tie on `x > 50000`. Without `stat4` (never analyzed, or analyzed by our own `stat1`-only `ANALYZE`) nothing changes, which is also sqlite3's behaviour. New fixture `tests/fixtures/btrees/stat4_range.db` (sqlite3-analyzed). UPDATE's row seek and equality probes are not gated; the sqlite-rs CLI still uses its own `stat1`-only loader and does not yet see this (follow-up there).

## [0.112.0] - 2026-09-18

### Changed

- **Aggregate expressions** (#496): `SUM(amount * 2)` (an expression inside an aggregate's argument) and `SUM(x) * 2` / `SUM(x) + SUM(y)` / `SUM(x) / COUNT(*)` (an aggregate composed with arithmetic, including alongside `GROUP BY`) now compile, where both were previously rejected. `vm::batch::AggPart` gains `Hidden` (a merged aggregate that isn't itself an output column) and `Expr` (a post-`Combine` scalar op over two `AggOperand`s), generalizing the `Avg` finalize hook to arbitrary aggregate arithmetic; both stay `Copy` so the AOT-emitted `const PROGRAM` path is unaffected. Nested aggregates and aggregates in `WHERE` are still rejected, each with its own dedicated error message.

## [0.111.1] - 2026-09-18

### Fixed

- **`INSERT ... VALUES` stored the wrong value for any non-first column whose expression needed a temporary register** (#497): `VALUES (3, -1)` stored `3|1`, `VALUES (-7, -8)` stored `-7|8`. `compile_insert` handed `MakeRecord` the first column's register and assumed the rest followed contiguously, but `compile_value` leaves a unary minus (`0 - 1`), arithmetic or function-call result past the temporaries it allocated, so the record read a temporary instead. When the collected registers are not one contiguous run they are now `Copy`-staged into a fresh block first (the `SELECT` path already did this before `ResultRow`); an all-literal row emits exactly what it did before. Present since at least 0.101.0; a `CHECK (b > 0)` on such a column was silently bypassed because the corrupted value satisfied it. Verified against the sqlite3 3.53.4 oracle.

## [0.111.0] - 2026-09-18

### Changed

- **Implicit-group aggregates walk a range seek as a covering index** (#484): `codegen::row::select::aggregate::try_compile_direct_agg_scan` used to snapshot every table column off the first matching row and, on the #279 index range seek, `IdxRowid` + `SeekRowid` back into the table for every matching index entry -- `SELECT count(*) FROM t WHERE x > (SELECT avg(x) FROM t)` on an indexed `x` did ~415K random b-tree seeks on the 50 MB bench fixture. The snapshot now reads only the columns the select list/`HAVING` reference (#506's `columns_needed_for_projection`, shared with the `GROUP BY` path), and when every such column is a key column of the seek's index the per-entry body reads off the index cursor with no table lookup at all: sqlite3's `SeekGT`/`AggStep`/`Next` shape, reported by `EXPLAIN QUERY PLAN` as `SEARCH t USING COVERING INDEX ix (x>?)` exactly as the oracle does. A/B against main on `benchmark/perf/sqlite-rs`'s fixtures, wall clock through the sqlite-rs CLI: `agg_subquery` 50 MB 529 ms -> 118 ms (sqlite3: 36 ms); 1 MB unchanged at ~25 ms (process start dominates). `in_subquery_agg_outer` is a table scan with a `Found` probe and only loses the dead first-row loads; its remaining gap is per-row decode (#485). The selectivity heuristic for a *non*-covering range seek (issue item 3) is not part of this change. Results verified against the sqlite3 oracle on both fixtures, including bare-column, `HAVING`, REAL and rowid-alias probes.

### Fixed

- **`make test-mcdc` discharged nothing** : the harvest tool runs a bare `cargo test`, which failed to compile `unit_engine_resolve_public_api` (it uses `FakeClock`, gated behind the non-default `storage-test-support` feature), so no tagged test ever ran and every obligation reported 0 vectors. Rather than gating that target off (which would hide its three tagged tests from the harvest), a test-only self-dependency in `[dev-dependencies]` enables the feature for test builds. The gate is back to 155/155: the 19 vectors for the six decisions that landed untagged while it was dark (`engine::stream::requests`, `vm::batch::{projection_only_columns, run_morsels_ordered, typed_arithmetic}`, `vm::combine::combine_partials`, `vm::engine::run`) are tagged in this release.

## [0.110.0] - 2026-09-18

### Changed

- **`GROUP BY` partials are pre-aggregated per pool worker before the final `Combine`** (#488): `run_parallel` emitted one partial row per group *per segment*, so the final merge saw `segments x groups` rows (8.2M on the parity `GROUP BY customer_id`). Each morsel-pool worker now folds every segment it claims into its own incremental `vm::combine::Combiner` (new `run_morsels_fold`, same dynamic rebalancing) and hands back one still-mergeable partial, so the final merge sees at most `threads x groups` rows -- the thread-local aggregation DuckDB and Velox rely on, applied at the merge boundary without touching the VM. Output order is unchanged and deterministic: groups carry their smallest `(segment, row)` first-seen key through the partials and the final merge sorts by it. Gated at 4 segments per thread (`PREAGGREGATE_MIN_SEGMENTS_PER_THREAD`); below that the direct merge is kept, since pre-aggregation measured 14% slower with ~1 segment per worker. Interleaved A/B/A on the tracked bench, 12 threads: 64 segments x 20K groups 24.1 ms -> 17.4 ms; 16 x 100K unchanged. A non-`Int` key in a later chunk now demotes the integer table in place rather than redoing the merge.

## [0.109.0] - 2026-09-17

### Changed

- **`Combine` merges a single-integer `GROUP BY` key through an open-addressing table** (#478 phase 2): `vm::combine::combine_chunks` decides the key specialization from the first chunk (as ClickHouse's `chooseMethod` and DataFusion's `GroupValuesPrimitive` do from the schema). A single `Int`/`Null` key now goes through `IntKeyTable` -- linear probing over a power-of-two `u32` slot array into a dense `(key, group id)` vector, load factor <= 1/2, presized from the first chunk, keys hashed with MurmurHash3's 64-bit finalizer so high-bit entropy (timestamps, shifted ids) reaches the slot index -- instead of two SipHash rounds and a `Value` comparison per partial row. Composite, string and float keys keep the generic hash-then-verify path; both share `merge_columns`. Tracked bench `engine::run GroupReduce+Combine` (16 segments x 100K groups), A/B/A against main: 62.2 ms -> 37.6 ms, the merge tail alone ~36 ms -> ~12 ms (~7 ns per partial row, from ~52 ns before 0.108.0). Phase 3 (partitioned parallel merge) and #488 (per-thread pre-aggregation) follow.

## [0.108.0] - 2026-09-17

### Changed

- **`Combine` merges partial `GROUP BY` aggregates column-major, straight off the per-segment chunks** (#478 phase 1): `vm::engine::run` no longer transposes every partial row to `Vec<Vec<Value>>` before merging. The new `vm::combine` module hashes the key columns column-wise (#440), maps rows to groups through the same hash-then-verify table `GroupReduce` uses (#439), and merges each aggregate column into its accumulator with one dispatch per column-chunk instead of per row. The merge semantics (`Null` identity, checked-`i64` `COUNT`, `f64` `MIN`/`MAX`, `AVG`'s `(sum, count)` pair, #404) are now defined once in `slot_ops`/`merge_slot`, with `finalize` built on the same functions and a differential test pinning the two. Group output order is unchanged. Measured on 82 segments x 100K groups: merge tail 431 ms -> 184 ms; new tracked bench `engine::run GroupReduce+Combine` (16 x 100K) 119.0 ms -> 70.9 ms A/B/A against main. Phases 2-3 (typed single-`Int` key, partitioned parallel merge) and #488 (per-thread pre-aggregation) follow.

## [0.107.2] - 2026-09-17

### Changed

- **`Combine` merges `GROUP BY` partials by a typed key, not a stringified one** (#478): `vm::engine::finalize` used to build a `Vec<String>` of each group-key column's `Display` output, join it, and hash that string per row -- at 100K groups across 82 segments (8.2M rows) this was the whole query's cost. Now uses the hash-then-verify shape already established for `GroupReduce`'s own grouping (`#439`/`#440`): a `HashMap<u64, Vec<usize>>` bucketed by a variant-tagged hash computed over a borrowed key slice, allocating an owned key only for a genuine new group. Also fixes a latent correctness bug the stringified key had: `Int(1)` and `Str("1")` both format to `"1"` and used to merge into the same group. Applied to the `DISTINCT` dedup pass too. A synthetic 82-segment x 100K-group benchmark went from 1285ms to 337ms for the merge alone; full parity with DuckDB needs a larger column-major merge rewrite tracked separately in #478.

## [0.107.1] - 2026-09-17

### Fixed

- **Shared Parquet row-group decode boundary extracted to `storage::column::decode`** (#467): `engine::column::RowGroupSegment` and column-rs's own `RowGroupSegment` each maintained a divergent `PhysicalType` -> `Column` decode dispatch, so storage-layer work (#457, #461, #472) landed in db-core's loader but never reached column-rs's CLI or the parity benchmarks that measure it. `decode_column_full`/`decode_column_at` and their `Column`-building helpers now live in `storage::column::decode` (gated behind the `vm-batch` feature), the one place both loaders should call; `engine::column::RowGroupSegment` calls them directly with no behavior change. column-rs adopting these functions is a follow-up tracked separately.

## [0.107.0] - 2026-09-17

### Added

- **`decode_column_at` builds typed `Column`s directly** (#472): the positional/projection-only decode path (ADR-0026's phase-2b, used after a `WHERE` filter to decode only surviving row positions) now reuses `decode_column_full`'s `int_column`/`float_column`/`bool_column`/`str_column` conversion helpers instead of rebuilding a `Vec<Value>` per row, closing #461's remaining positional-path gap. Documents the shared `Vec<Option<T>>` -> `Column` conversion contract in `vm::column`'s module doc. Dictionary-encoded string columns remain eager-decode-only for now.

## [0.106.1] - 2026-09-17

### Fixed

- **`count(*)` over a filter is now selectivity-independent** (#474): `Opcode::Reduce` unconditionally called `resolve_selection`, which eagerly compacted every live register down to the surviving rows -- O(live registers x survivors). A bare `COUNT(*)` (`src: None`) never reads register contents, only the survivor count, so it paid that cost for nothing (a 10M-row scan cost ~9x more at 50% selectivity than at 1%). It now peeks the pending selection's index count instead of resolving it, leaving the selection intact for any other aggregate in the same `SELECT` list that still needs its own source register compacted; a new `scalar_registers` set lets `resolve_selection`/`Emit` tell a `Reduce`'s scalar result apart from a genuine one-row column.

## [0.106.0] - 2026-09-17

### Added

- **Parquet numeric/bool/string columns decode straight into `Column`** (#461): `engine::column`'s `decode_column_full` (the whole-row-group path shared by the eager load and #460's predicate phase) now builds `Column::Int`/`Float`/`Bool`/`Str` directly for `Int64`/`Int32`/`Double`/`Float`/`Boolean`/plain-string columns, extending #457's dictionary-string pattern to every physical type. The `Vec<Value>` intermediate (`Decoded::Values`) is gone from that path; `decode_column_at` (#460's positional projection-only path) is unchanged for now. Consumption by `GroupReduce`/`HashBuild`/`HashProbe` (#428/#435) and the Lance spike/Arrow-contract doc from #461's full scope remain follow-up work.

## [0.105.0] - 2026-09-17

### Added

- **Late materialization for filtered projections** (#460): a filtered projection previously decoded every projected column for every row before evaluating the `WHERE` predicate. `codegen::batch::compile` now defers a non-predicate projected column's `LoadColumn` past `Filter` instead of before it (reusing `Emit`'s existing lazy-selection handling), and `engine::column::RowGroupSegment::load` decodes predicate columns for the whole row group, runs the filter prefix in a scratch `Vm` to get the surviving row positions, then decodes the remaining projected columns only at those positions via new `RowGroupReader::read_*_column_at` methods. `GROUP BY`/no-`WHERE` queries keep the prior eager decode, unchanged. Dictionary-encoded string columns (#457) fall back to plain decode in the projection-only path for now; page-level skip-decode and `filter_1pct`/`filter_50pct` parity validation against the 10M-row suite are follow-up work (ADR-0026).

## [0.104.0] - 2026-09-17

### Added

- **Row-group pruning from footer statistics** (#458): `storage::column::parquet::footer` now parses per-column-chunk `Statistics` (min/max/null_count/distinct_count, preferring the newer `min_value`/`max_value` fields over the deprecated `min`/`max`), exposed via `RowGroupReader::column_statistics`. `engine::column` uses them to skip building a `RowGroupSegment` for any row group a `WHERE` clause's footer statistics prove cannot match (simple column-vs-literal comparisons combined with AND/OR); malformed or missing statistics, or an unrecognized predicate shape, always fall back to "may match" rather than an incorrect skip (ADR-0000: pruning is correctness-neutral). Wiring pruning into `run_parallel_top_n`'s live top-N heap threshold is tracked separately (#464).

## [0.103.0] - 2026-09-16

### Added

- **Dict-encoded Parquet columns reach `vm::batch` as `Column::Dict`** (#457): `RowGroupSegment::load` now tries `ParquetFile::read_string_column_dictionary_indices` before the plain string decode, so a `PLAIN_DICTIONARY`-encoded column materializes as dict + per-row `u32` code + validity bitmap instead of one owned `String` per row (a column with no dictionary page, or one that falls back to `PLAIN` mid-chunk, still decodes the plain way). `GroupReduce` gained a matching fast path for a single dict group-by column: groups by integer code through a `dict.len()`-sized array instead of hashing decoded strings, decoding only the codes that end up in the output. Row-group skip-on-literal (mirroring `OwnedColumn::dict_contains` for stream segments) is left as a follow-up.

- **`vm::engine::run_streaming`** (#456): streams a plain scan/filter/projection's chunks to a caller-supplied sink, in segment order, as each becomes ready, instead of collecting the whole result into one `QueryOutput` first (`vm::batch::run_parallel_streaming`/`run_morsels_ordered` underneath). Peak memory is bounded by an ordering window (twice the worker pool), not the result size. Only programs `vm::engine::run`'s own identity-Combine short-circuit accepts can stream -- an aggregate, `DISTINCT`, `ORDER BY`, or `LIMIT` needs every segment's output before producing even its first row, so `run_streaming` returns the new `VmError::NotStreamable` for those; `vm::engine::is_streamable` lets a caller check this up front, before committing to a sink (e.g. printing a header), rather than discovering it mid-stream. `run` and its output are unchanged. column-rs's `-c` one-shot mode is the first consumer (t-rust-db/column-rs#34). Measured on the 10M-row parity suite: `scan` 1015 -> 970 ms / 1272 -> 524 MB, `filter_50pct` 1039 -> 990 ms / 1377 -> 592 MB; `group_by` (not streamable) flat at 217 MB.

### Fixed

- **`ORDER BY ... LIMIT` heaps indices, not rows** (#459): `run_parallel_top_n`/`top_n_reduce` heaped a full `Vec<Value>` row per candidate, fed by `QueryOutput::rows()` -- which materializes one row (every output column) for *every* input row regardless of whether it survives. `top_n_reduce_output` now reads chunks directly, inspecting only the `ORDER BY` column's cell per candidate, and gathers a row's other columns only once it's actually pushed onto the heap; the cross-segment merge carries already-extracted candidates instead of flattening and re-heaping materialized rows, so a winning row is gathered exactly once. Measured on the 10M-row parity suite (A/B/A against this version's own baseline): `order_by` 51.8-53.3 ms / 222-227 MB -> **31.4 ms / 219 MB** (DuckDB parity 0.98x, meeting the <=1.0x stretch goal, up from 1.64-1.80x).

## [0.102.1] - 2026-09-16

### Fixed

- **Bare aggregates always emit exactly one row** (#452): an aggregate query with no `GROUP BY` was planned as a `GroupReduce` with zero key columns, which over zero surviving rows found zero groups and emitted no row where SQL requires one (`COUNT(*)` = 0, every other aggregate `NULL`). The planner now emits one `Opcode::Reduce` per aggregate (`GROUP BY` keeps `GroupReduce`), which also puts bare aggregates on the typed `Reduce` fast path (#433). Two latent bugs on that path fixed alongside: `Reduce` `COUNT(*)` counted `batch.num_rows`, ignoring a preceding `Filter` (the VM now records the applied selection's row count), and the cross-segment merge folded all-`NULL` `SUM`/`AVG` partials into `0.0` (`NULL` is now the identity). Measured on the 10M-row parity suite against v0.102.0 (interleaved A/B): `agg_count` 16.1 -> **2.9 ms**, `agg_sum` 30.9 -> **19.1 ms**; all other outputs identical.

## [0.102.0] - 2026-09-16

### Changed

- **Query output is chunked and column-major** (#436, **breaking**; second attempt, replacing the reverted 90c8571): `Vm::run`/`take_output`, `run_parallel`/`run_parallel_top_n`, and every `vm::engine` execution entry point (`run`, `run_join`, `run_join_segments`, `run_multi_join_segments`, `finalize`) return the new `QueryOutput` -- an ordered list of `Chunk`s (`Vec<Arc<Vec<Value>>>`, one per `Emit`), never concatenated -- instead of row-major `Vec<Vec<Value>>`. `Opcode::Emit` pushes one chunk (with no selection the emitted `Arc` *is* the register's column, so a projection copies nothing); the cross-segment merge moves chunk pointers; `vm::engine::run` returns the chunks untouched when the trailing `Combine` has no work; top-N is fed lazily and holds O(limit). `QueryOutput::rows()`/`into_rows()` give row shape where a consumer needs it (documented as per-row-allocating; walk `chunks()` for large results). The `codegen::batch::emit` AOT templates iterate `rows.rows()`; `engine::QueryResult` stays row-major. Measured on the 10M-row parity suite against v0.101.7 (interleaved A/B): `filter_50pct` 635.7 ms / 1151 MB -> **556.2 ms / 973 MB**, `scan` 16.2 -> 10.2 ms / 43 -> 25 MB, `order_by` 73.8 -> 54.4 ms / 325 -> 225 MB; `filter_1pct`, `join`, `group_by`, aggregates flat. Consumers must read large results by chunk, not by row: column-rs's `-c` printer does (see column-rs#33).
- CI: the `Emit compile guard` job reads a `column-rs-ref: <branch>` line from the PR body, so a breaking change to the query-output surface can be compiled against its companion column-rs branch (#447 follow-up).

## [0.101.7] - 2026-09-16

### Fixed

- **Row hashing holds a `u64` per row, not a 72-byte `DefaultHasher`** (#444): `hash_columns_by_row` (#440) kept one live SipHash state per row for its whole column-by-column fold -- 8.85 MB per 122,880-row segment, ~106 MB across in-flight morsels, for 8 bytes of output per row. Each value is now hashed one-shot on the stack and mixed into a `Vec<u64>` accumulator (`mix_hash`); build and probe share the one function so join hashes stay consistent. Measured on the 10M-row parity suite: `group_by` 80.8 ms / 282 MB -> 69.8 ms / 221 MB, `join` 130.8 ms / 352 MB -> 120.5 ms / 242 MB.

## [0.101.6] - 2026-09-16

### Changed

- **Reverted #436's column-major query output** (v0.101.5): it regressed the query it targeted. On `benchmark/parity/column-rs` at 10M rows, `filter_50pct` (5M rows out) went from 651 ms / 1042 MB to 813 ms / 2350 MB; pre-sizing the merge recovered only ~30 ms and ~350 MB of that. Isolating by output column count shows the cost scales with *columns* (~313 MB and +0.11 s per column, against ~124 MB and +0.06 s row-major), not with the merge transient -- `run_morsels` materializes every segment's output before merging, so peak is O(result) either way and the columnar layout makes each of those allocations large and contiguous. `scan` (1K rows out) was 14% faster and 6 MB smaller, so the premise holds; a correct implementation needs a streaming merge rather than an accumulating one. Reopened as #436.

## [0.101.4] - 2026-09-16

### Fixed

- **`GroupReduce` fuses into `HashProbe` for a join that feeds only a `GROUP BY`/aggregate** (#441): `HashProbe` followed directly by `GroupReduce` (no `Map`/`Filter`/`Window` between them) is detected at plan time and compiled into a single `Opcode::HashProbeGroupReduce` instead -- matches fold straight into a per-group streaming accumulator, never materializing a joined row per match or rebuilding an intermediate `Batch`. Every other join shape (non-aggregate, or with a `WHERE`/projection between the probe and the reduce) is unchanged, byte-for-byte in `explain_opcodes`.

## [0.101.3] - 2026-09-16

### Fixed

- **`GroupReduce`/`HashBuild`/`HashProbe` hash group/join keys column-wise instead of materializing a row key** (#440): all three built a row-shaped key (a `Vec<Value>` gathered from each key column) before hashing or comparing it. They now hash column-wise into a per-row `u64` (one tight loop per column, no row key ever materialized) and, for `HashBuild`/`HashProbe`, store the build side's key/payload data column-major (one `Vec<Value>` allocation per column, not per row) behind a hash-only index that resolves collisions -- and NULL-never-matches join semantics -- by comparing the actual column values. `GroupReduce`'s own semantics (`NULL` groups together) and #439's probe-before-insert are unchanged, just fed by the new column-wise hash. ~20% faster `HashProbe` (`benches/vm_opcodes.rs::bench_batch_hash_join`, 1% dimension, `Str` payload).

## [0.101.2] - 2026-09-16

### Fixed

- **`GroupReduce` probes by hash before allocating a key** (#439): the hot loop built and cloned a `Vec<Value>` group key (with a `String` clone per `Str` key) for every input row, even though almost every row lands in a group that already exists. It now hashes the row's key columns in place and probes a hash-bucketed map with a borrowed comparison, only allocating an owned key when the row's group is genuinely new -- ~2x faster on low-cardinality `GROUP BY`s (100 groups: 189us -> 100us; 1% of rows: 177us -> 85us).

## [0.101.1] - 2026-09-16

### Fixed

- **`SELECT *` no longer fails on flattened JSONL nested-object fields** (#437): `StreamEngine::requests()` treated a flattened dot-path field name (e.g. `req.method`, one level of nesting per `storage::stream::jsonl`) as a qualified `table.column` reference, stripping the `req` prefix and failing to find a field literally named `method`. The literal full name is now checked before falling back to `split_qualified`'s table-prefix stripping, so both `SELECT *` and an explicit `SELECT req.method` resolve correctly; the genuinely-qualified case (`log.severity`, cross-mode joins, #317) is unaffected.

## [0.101.0] - 2026-09-15

### Added

- **`Reduce` (global aggregate) ported to `Column` dispatch** (#433, epic #130 child 4, slice 3): `Vm::typed_reduce_values` computes `COUNT`/`SUM`/`AVG`/`MIN`/`MAX` directly over a typed `Int`/`Float` source column's packed buffer + validity bitmap, matching `reduce_values`'s output shape exactly (`Sum`/`Avg`/`Min`/`Max` always `Float` even over `Int` data, `Count` stays `Int`, all-invalid yields `Null` not `0`) -- differentially tested against the existing `Vec<Value>` path. `GroupReduce`'s per-group accumulators and `Combine`'s cross-segment merge (which operates on already-emitted rows, not registers) are explicitly deferred to future slices.

## [0.100.0] - 2026-09-15

### Added

- **`Map` arithmetic (`Add`/`Sub`/`Mul`/`Div`) ported to `Column` dispatch** (#431, epic #130 child 4, slice 2): `Vm::typed_arithmetic` computes over packed `Int`/`Float` buffers into a genuine typed `Column` result when both `Map` operands are typed registers, honoring the same NULL-propagation and Int-stays-Int-unless-`Div` promotion rules as the existing `Vec<Value>` path -- differentially tested against it. Also materializes into `registers` (the #429 `LoadColumn` safety-net pattern) so un-migrated opcodes keep working. Scoped to both operands being typed columns; a `LoadConst`-broadcast literal falls back to the general path correctly, just without the fast path.

## [0.99.0] - 2026-09-15

### Added

- **`LoadColumn`/`Filter` ported to `Column` dispatch** (#429, epic #130 child 4, slice 1): `Batch` gains `typed_columns` (a `Column`-backed representation, additive alongside the existing `Vec<Value>`-backed `columns`) and `Batch::with_typed_column`; `Vm` gains `typed_registers` alongside `registers`. `LoadColumn` populates both -- `registers` as a materialized safety net every un-migrated opcode keeps reading unchanged, `typed_registers` for two new fast paths: `Filter`'s predicate mask reads a typed `Bool` column's bitmap+data directly instead of matching `Value::Bool(true)` per element, and `Map`'s `Eq`/`Ne` against a `Dict` column and a resolved string literal compares dictionary codes instead of decoding every row (`Vm::dict_literal_compare`) -- the concrete `= 'kern'` case #399 names, differentially tested against the equivalent `Str`/`Vec<Value>` path. `resolve_selection` clears `typed_registers` on compaction to avoid a stale, wrong-length shadow. `LoadColumn` still materializes `Vec<Value>` eagerly rather than lazily, since several un-migrated opcodes (`Emit`, `HashProbe`, `GroupReduce`, `HashBuild`, `Window`) manipulate `Arc<Vec<Value>>` directly in ways a lazy register model would need much larger changes to support correctly -- left to a later #428 slice.

## [0.98.0] - 2026-09-15

### Added

- **Typed `Column`/`Bitmap` core types for `vm::batch`** (#425, epic #130 child 3): `vm::column::Bitmap` is a hand-rolled validity bitmap over `Vec<u64>` (no `unsafe`, no dependency); `vm::column::Column` is an enum over packed `Int`/`Float`/`Bool` buffers, Arrow-style offsets+data for `Str`, and `Dict{dict, indices}`, mirroring `storage::stream::segment::OwnedColumn`'s variant set but with a real per-column bitmap instead of per-element `Option<T>`. `impl From<Vec<Value>> for Column` gives existing `Batch::with_column` test call sites a migration path. Per ADR-0025's constraint on this child, nothing assumes a source-level static schema: the same column name can seal as `Int` in one `Column` and `Str` in the next. Additive only -- does not change `Batch`'s field or wire any `Opcode` kernel to `Column` (that's child 4). Prerequisite for #399 (stream storage boundary), found missing while validating that issue.

## [0.97.0] - 2026-09-15

### Changed

- **`GROUP BY`'s accumulator reset compiled once, not fused** (#409): Pre #396, `GROUP BY` folded a new row into the accumulator and reset at the same time via an `AggStep` with `p5=1` (fused reset+fold). This makes the oracle's actual behavior (SQLite's `select.c` emits two separate instructions: `Gosub` to a reset subroutine, then a plain `AggStep` to fold) impossible to match. New `Opcode::AggReset` (`src/vm/row/program.rs`, `vm.rs`, `explain.rs`) splits them: before each loop and on each group boundary, `AggReset` clears the accumulator state (via `clear_agg_context`, the same method `AggFinal` uses), then a plain `AggStep(p5=0)` folds the row. `emit_reset_gosub`/`emit_reset_subroutine` (mirroring #396's flush pattern) factor this into one Gosub-called subroutine, called from both pre-loop and boundary sites in both `compile_grouped_scan` and `try_compile_index_ordered_group_by`. Behavior unchanged; shape now matches SQLite exactly.

## [0.96.0] - 2026-09-15

### Added

- **Local DuckDB oracle for the column engine; stream's oracle-by-construction pattern named** (#406): `examples/column_oracle.rs` + `tools/check_column_oracle.sh` (`make check-column-oracle`) diff the column engine's query results against DuckDB reading the same Parquet fixtures, across Zstd/Snappy/uncompressed codec variants -- deliberately local-only, never CI, since a DuckDB binary is a new external dependency this charter does not accept onto the machine that runs untrusted PRs. ADR-0000 §(f) and ADR-0015 also name stream's existing "oracle by construction" pattern (`tests/unit/engine_stream_public_api_test.rs`'s `oracle()`/`json_status_oracle()`), which independently re-derives the expected answer from raw fixture bytes instead of trusting the engine's own parser.

## [0.95.2] - 2026-09-15

### Fixed

- **`SegmentSummary` now covers predefined fields** (#417): `severity`, `facility`, `timestamp`, and `observed_ts` are dedicated `Segment` struct fields, not part of the Tier-3 `field_names`/`field_cols` `SegmentSummary.columns` was built from -- so `SUM`/`MIN`/`MAX(severity)` over a query reaching into evicted history silently dropped the evicted majority's contribution instead of erroring, contradicting `SegmentSummary`'s own doc comment. `Segment::from_batch` now summarizes these under the same SQL-facing names `storage::stream::adapter` already uses. Found while extending the segment-split invariance harness (#404) to stream eviction.

## [0.95.1] - 2026-09-15

### Fixed

- **`AVG` never merged across segments** (#404): `vm::engine::merge_rows`'s `AggPart::Avg` arm was a no-op -- codegen allocates two registers (sum, count) per `AVG` but pushes only one `AggPart::Avg` entry into `agg_parts`, and the merge loop was bounded by `agg_parts.len()`, so the count register (past the end of that list) was never merged across segments. With more than one segment, `AVG` silently returned the first segment's local average instead of the true merged mean. A related bug in `finalize_row`/`merge_rows` indexed the emitted row by its position in `agg_parts` instead of the row's own (wider, once an `AVG` is present) cursor, silently corrupting or dropping whichever aggregate followed an `AVG` in the same query. Both fixed with a running row cursor. Caught by a new segment-split invariance harness (`tests/integration/segment_split_invariance_test.rs`, ADR-0025 obligation 1).

## [0.95.0] - 2026-09-15

### Added

- **Stream profile charter gate** (#403): `tools/check_sqlite_profile.py` is generalized from a hardcoded SQLite-only script into a named-profile gate (`PROFILES` table); SQLite profile behavior/output is unchanged. Adds a `stream` profile for the log engine's execution mode (`parser-column, vm-batch, vm-stream, codegen-batch, codegen-stream, storage-stream, engine-stream`), currently passing with zero third-party dependencies and zero `unsafe` sites. ADR-0000's invariants (a) dependency closure and (b) `unsafe` carve-outs now apply to both profiles; invariant (c) mode isolation stays SQLite-only, since the stream profile legitimately compiles `vm::batch`/`codegen::batch`. New `make check-stream-profile`, wired into `make ci` and CI (`.github/workflows/ci.yml`).

## [0.94.0] - 2026-09-15

### Changed

- **`GROUP BY`'s output epilogue compiled once, not twice** (#396): `compile_grouped_scan`/`try_compile_index_ordered_group_by` shared a `flush_group` helper but each inlined a fresh copy of its bytecode at both the group-boundary flush site and the end-of-scan tail flush, doubling the `AggFinal`/`MakeRecord`/`OpenPseudo`/projection block in every grouped query's program. New `Opcode::Gosub` (`src/vm/row/program.rs`, `vm.rs`, `explain.rs`) plus the previously-dead `BeginSubrtn`/`Return` now factor that block into one subroutine, called from both sites -- mirroring SQLite's own `select.c`/`addrOutputSubroutine`. `Return`'s calling convention changed to match SQLite exactly (`r[p1] + 1`, not the address stored verbatim) now that a real `Gosub` emitter exists.

## [0.93.0] - 2026-09-15

### Changed

- **`engine::OpcodeRow` carries `comment` and `is_finalize`** (#401): all four producers (row, batch/column, stream, cross-mode resolve) now pass an instruction's annotation and its `Combine`-barrier marker through as real fields instead of folding `comment` into `operands` as a trailing `; comment` (batch/stream/resolve) or dropping it outright (row). `is_finalize` is `true` exactly on a batch program's `Combine` row -- the boundary between the parallel per-segment phase and the sequential merge phase (ADR-0007, #48) -- and always `false` for row/stream sections, which have no `Combine` opcode. `p5` (row programs' opcode flags) remains unsurfaced, documented on the struct. Breaking for any external constructor of `OpcodeRow` literals; db-studio is the only known consumer.

## [0.92.0] - 2026-09-14

### Added

- **Multi-way SQLite joins in a cross-mode query** (#394): a cross-mode `SELECT` can now join the driving stream table against N SQLite lookup tables in one query (`log JOIN hosts ON log.host_id = hosts.id JOIN users ON log.user_id = users.id`), not just one. This is a star join -- every join key must be a column of the driving table `log` (no lookup table is joined against another lookup table's payload), and `log` must be written first (`resolve_multi_sides`), unlike the single-`JOIN` case which accepts either grammar position. New: `codegen::batch::compile_cross_mode_multi_join`, `vm::engine::{MultiJoinProgram, JoinBuildSide, run_multi_join_segments}`. `EXPLAIN`/`explain_opcodes` for this shape are not yet supported (reject with `ErrorKind::Unsupported`); the single-`JOIN` path (`resolve_sides`/`compile_join`/`compile_join_build_side`) is unchanged.

## [0.91.0] - 2026-09-14

### Added

- **`ScanSource` opcode for cross-mode joins** (epic #382, ADR-0024, #384-#388): the build side of a cross-mode join now compiles into the VM's own opcode program instead of being materialized in Rust before execution reaches `vm::engine`. `Opcode::ScanSource(ScanSource)` (`RowTable`/`Stream`/`InMemory`) is emitted by `compile_join` ahead of `HashBuild`; `vm::engine::run_join_segments` resolves `RowTable`/`Stream` sources through a caller-supplied `ScanSourceResolver` seam (keeping the engine/VM layering intact -- `vm::engine` still can't reach a SQLite table or a stream engine directly), while `InMemory` needs no resolver.
- **`engine::resolve::CrossModeEngine`**: a real `Engine` impl spanning a stream-driven SQLite-lookup join (ADR-0019) or a windowed stream-stream self-join (ADR-0022) -- pure routing onto the existing `run_query`/`run_stream_stream_query`/`explain_plan`/`explain_stream_stream_plan` free functions. This closes the gap db-studio#54 had to work around: cross-mode queries now have a real `explain_opcodes`, not just `explain_plan`.
- **`OpcodeSection::lane`**: a `&'static str` ("row"/"batch"/"stream") naming which physical engine executes that section's opcodes. Single-engine `Engine` impls set it to their own fixed mode; `CrossModeEngine::explain_opcodes` sets it per section -- a stream/SQLite join's build side is labeled "row" (the SQLite lookup scan), its probe side "stream" (the driving tail), its body always "batch" (every join executes on `vm::batch` regardless of either side's origin).
- **SQLite as the driving side of a cross-mode join** (ADR-0021, #371): `engine::resolve` now accepts a join written with the SQLite lookup table as the `FROM` table (`hosts JOIN log`, not `log JOIN hosts`). INNER JOIN only -- a LEFT JOIN with SQLite driving remains unsupported (would need RIGHT JOIN semantics, #368).

### Fixed

- **`temp_log` fixture path collision** (#381): a per-call atomic counter makes every call's path unique, fixing a parallel-test file race that intermittently corrupted `windowed_self_join_inner_join_drops_unmatched_rows` and its siblings (previously named by pid + a fixed label alone).

### Changed

- `compile_join` takes a `BuildSourceKind` (`RowTable`/`Stream`/`InMemory`), supplied by the caller (`compile_join` classifies a query purely from its `Select` AST and has no way to tell a SQLite lookup table from a stream table or an already-materialized batch on its own).
- Benchmarked whole-table vs. key-restricted lookup materialization (#373, `benches/cross_mode_lookup.rs`) -- informs future work on `cross_mode::scan_table_as_batch`, no behavior change.

## [0.90.1] - 2026-09-13

### Added

- **SQLite as the driving side of a cross-mode join** (ADR-0021, #371): `engine::resolve` now accepts a join written with the SQLite lookup table as the `FROM` table (`hosts JOIN log`, not `log JOIN hosts`), expanding the scope of cross-mode joins. INNER JOIN only: a LEFT JOIN with SQLite driving remains unsupported (would need RIGHT JOIN semantics, #368). Implements `codegen::batch::compile_join_build_side` to compile build/probe roles based on table identity rather than grammar position.

### Fixed

- **Public API snapshot** (`tests/public_api.md`): removed spurious platform-specific `vfs::fcntl::fsync` entry reintroduced during conflict resolution of the rebase (macOS-only, CI runs on Linux).
- **`explain_plan_shares_run_querys_side_resolution_errors` test**: updated assertion to reflect that `hosts JOIN log` (SQLite as FROM/driving side) is now accepted for INNER JOIN, so swapped the case to the still-rejected shape (SQLite driving a LEFT JOIN).

## [0.90.0] - 2026-09-13

### Added

- **Windowed stream-to-stream joins** (#372, ADR-0022): `engine::resolve::run_stream_stream_query`/`explain_stream_stream_plan` join two `StreamEngine` sources via a self-join (`FROM log AS a JOIN log AS b ON ... SINCE ...`), reusing `compile_join`/`HashBuild`/`HashProbe` unchanged. A `SINCE`/`UNTIL` time window is required -- with no bounded side to hash-build from, the window is what makes the join finite; an unwindowed or non-time-scoped (`LINES`/`BYTES`) self-join is rejected as `ErrorKind::Unsupported`. New `StreamEngine::segments_in_range` bounds a side's segments by observed-time overlap, and the build side's row count is capped to bound memory use.
- **Self-join column disambiguation** in `parser::column::validate_select`: when two aliases in one `FROM`/`JOIN` resolve to the same real table (a self-join), the alias-to-real-name rewrite is skipped for those aliases so `a.col`/`b.col` survive as written instead of collapsing onto the same name.

### Fixed

- **`engine::predicate`/`Engine::compile_predicate` (#369) feature gate**: was gated on `vm-batch` alone but also needs `codegen-batch` (`storage-stream` implies `vm-batch` transitively without implying `codegen-batch`, same root cause as db-core#356).

## [0.89.0] - 2026-09-13

### Added

- **`Engine::compile_predicate`/`CompiledPredicate::eval`** (#369): compile a bare boolean expression -- the same grammar `WHERE` already uses -- against the engine's current schema, then evaluate it repeatedly against individual already-materialized rows (as `Cell`s keyed by column name), with no file I/O or engine re-query. Reuses `codegen::batch`'s existing expression compiler, so `LIKE`/`GLOB` (#352) and anything else `WHERE` accepts work the same way here. Built for db-studio's highlight/dim/filter feature (db-studio#45).

## [0.88.2] - 2026-09-12

### Fixed

- **`SyslogParser::new()` hardcoded `year: 2024`** for RFC 3164 timestamp parsing (syslog lines carry no year field): silently wrong on any other year. Now derives the current year from the system clock via a dependency-free civil-from-days conversion.

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
- `sql-parser::column`: unary minus/plus (`-x`, `+x`) and `||` string concatenation (#34), growing column-rs's grammar toward DuckDB parity rather than SQLite's — `||` binds looser than `+`/`-`/`*`/`/` (DuckDB/Postgres precedence), deliberately not SQLite's own tighter-binding placement. `sql_expr` gains `BinOp::Concat`/`Expr::Neg`; `sql_vm::batch` gains `MapOp::Concat` (stringifies both operands) and `MapOp::Neg` (`Int`/`Float` negate, `Null` otherwise).

### Fixed

- `sql-vfs`: added the missing `[lints.clippy]` baseline (matching sqlite-rs's own and `sql-record`'s/`sql-header`'s) -- its own `#[allow(clippy::unwrap_used, ...)]` annotations assumed this was already enforced.
- `sql-vfs`: `cargo test -p sql-vfs` alone never built its own `src/bin/lock_probe.rs` helper binary (`cargo test` doesn't build sibling `bin` targets automatically) -- the merged `#14` test suite only ever passed locally because `--bins` had been built manually first. `Makefile`'s `test` target now builds `lock_probe` explicitly first, matching sqlite-rs's own Makefile.

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
