# ADR 0029: Packaging the write path — db-core features, `column-ingest`, readers in `tools`

## Status

Proposed (#TBD; builds on ADR 0027 and ADR 0028)

## Context

ADR 0027/0028 add a writer and a table directory format. Three ways of
feeding a table are wanted:

1. **Embedded** — an application links the writer and appends records.
2. **Piped** — events arrive on stdin (`app | …`, or a forwarder such as
   Vector or Fluent Bit writing JSON Lines to a pipe).
3. **Logfile** — an existing log file is followed (ADR 0027's logfile
   mode).

And three existing clients should read the resulting tables: column-rs
(query CLI), loglume (log TUI), db-studio (studio TUI).

Two facts shape where things go:

- **Clients have drifted apart on db-core.** At the time of writing db-core
  is v0.122; column-rs and loglume pin v0.114, db-studio v0.92. Every
  client carries its own `.cargo/config.toml` `[patch]` for sibling
  development (with the db-core#481 caveat). A feature that touches four
  clients means four PRs and four pin bumps.
- **A client's "table" is one file today.** column-rs's
  `QueryEngine::add_table` (`column-rs/src/query.rs`) mmaps one Parquet
  file and stores one `data` buffer and one `column_names` list; joins and
  window functions materialize that one file. A table directory is many
  files with differing schemas plus WAL pages that are not Parquet.

## Decision

### db-core owns all of it, behind three features

| Feature | Contents | Consumers |
|---|---|---|
| `storage-column-write` | Parquet writer (ADR 0027) | `ingest` |
| `ingest-read` | `manifest.json` parsing and invariant check; read-only WAL frame reader; `TableSnapshot` (segment list over Parquet row groups and WAL pages, schema union with NULL-fill, retry on a concurrent flush); `engine::column` opening a table directory as `Mode::Batch` | column-rs, loglume, db-studio, applications |
| `ingest` (implies `ingest-read`, `storage-column-write`) | `Table` (writer handle, ingest thread, WAL thread, flush, recovery), compactor (ADR 0028), `Input` sources | applications, `column-ingest` |

Readers never compile the writer. The writer API is synchronous (std
threads and channels, no async runtime):

```rust
let table = Table::open("events/", TableConfig::default())?; // LOCK + recovery
let lsn = table.append(record, Ack::Durable)?;
table.append_batch(&records, Ack::Durable)?;
let res = table.query("SELECT event_type, count(*) FROM events GROUP BY 1")?;
let snap = table.snapshot();                                 // TableSnapshot
table.ingest(Input::Lines(Box::new(std::io::stdin().lock())))?;
table.ingest(Input::Follow("/var/log/app.log".into()))?;     // logfile mode
table.close()?;                                              // fsync, stop threads
```

`Table` is a cloneable handle; `append` sends to the ingest thread.
`Input` lives in db-core, not in a client, because `Follow` is ADR 0018's
`LogFile` and logfile mode's offset-as-LSN is a storage concern:

```rust
enum Input {
    Lines(Box<dyn BufRead + Send>),   // stdin / pipe: JSON Lines or text lines
    Follow(PathBuf),                  // log file; the file is the WAL
}
```

A network listener (TCP, syslog) is a later `Input` variant.

### `column-ingest`: a thin binary

```
app | column-ingest events/
column-ingest events/ --follow /var/log/app.log
column-ingest events/ --config ingest.toml      # ADR 0028 [layout]
```

Argument parsing, config file, SIGINT/SIGTERM (stop intake, fsync,
exit), progress output. It contains no storage logic; everything it does
is reachable from the `Table` API above, so the embedded and CLI paths
cannot diverge.

### Clients live in one `tools` workspace

`t-rust-db/tools` is a Cargo workspace holding db-cli, column-rs,
sqlite-rs, loglume, trigrep and db-studio, imported with history (one
subdirectory per former repo, tools#1); column-ingest is added there. One
`Cargo.lock`, one db-core pin, one `[patch]` at the root, CI at the root.
Each crate keeps its own version, changelog, tags (`column-rs-v…`) and
gates (sqlite-rs keeps its assurance and supply-chain workflow);
`cargo install --git https://github.com/t-rust-db/tools <crate>` installs
one binary.

Staying separate: db-core (own audit and release cadence, ADR 0015/0025),
rainfrog-oracle (fork), benchmark and examples (deliberately decoupled
from client releases).

### Readers: a table is a file or a snapshot

column-rs's `Table` becomes `File(mmapped Parquet)` or
`Snapshot(TableSnapshot)`. `add_table` opens a directory containing
`manifest.json` as a snapshot named after the directory; query dispatch
runs over the snapshot's segment list instead of one file's row groups;
whole-table materialization for joins and windows goes through the same
list. column-rs still never writes. loglume and db-studio make the same
"source is a file or a table directory" change against the same
`TableSnapshot`, so each stays a thin adapter.

## Consequences

- Embedded, piped and logfile ingestion share one implementation and one
  set of tests in db-core.
- A cross-client feature is one PR in `tools` plus, if needed, one
  db-core release.
- column-rs's change is to its table abstraction, not a flag: joins,
  windows and the table registry all move from "one file" to "segment
  list". It is the largest client change and lands first.
- The `tools` migration requires bringing db-studio to the current
  db-core before the workspace builds with one pin.
- Old client repos are archived with a pointer to `tools`; open issues
  are transferred.
