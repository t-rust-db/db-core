# ADR 0043: Several SQLite files per connection: cross-mode files and `ATTACH`

Date: 2026-10-31

## Context

#924: a store keeps its row tables in several SQLite files next to its
column tables (DetACT: `session_live.db`, `machine.db`, `lookups.db`).
Before this, a cross-mode engine held exactly one SQLite file, and the row
engine had no `ATTACH`: the grammar marked `ATTACH`/`DETACH` and
`schema.table` names V10, the row grammar rejected a qualified `FROM`
table, and the row VM refused a nonzero database index in `OpenRead`'s
P3 (`ExecError::AttachedDatabasesUnsupported`).

## Decision

**Cross-mode engines hold named SQLite files.** A `CrossModeEngine`'s
SQLite side is a `Vec<SqliteFile>` (`name`, `engine`, `path`). Before
planning, `engine::resolve` binds each SQLite table a query names to one
file and strips the qualifier: a bare name binds when exactly one file has
the table, `name.table` picks the file, and an ambiguous bare name is an
error naming every file that has it. The batch planner keeps seeing one
bare name per table, so one query cannot read the same table name from
two files (Unsupported): the planner tells the sides of a join apart by
table name. Single-file constructors name their file after its stem. The
column dialect parses `schema.table` in `FROM` into `TableRef::schema`.

**The row engine attaches files the way sqlite3 does.**
`ATTACH 'file' AS name` / `DETACH name` are statements the engine runs
itself (no VM program). Database indexes follow sqlite3: `0` is `main`,
`1` is `temp`, attached files take `2..` in attach order. `temp` exists
but holds no tables (`CREATE TEMP` stays unsupported), so `temp.t` is
`no such table`, as in sqlite3 for an empty temp schema.

**`TableSchema` carries its database.** `TableSchema::database` (`0` for
`main`) is the P3 every `OpenRead`/`OpenWrite`/index open (and `Count`) of
that table emits. The row engine hands codegen one list: `main`'s tables,
then every attached file's under its bare name, then each again as
`x.t`. A bare name therefore finds `main` first and then each attached
file in order (sqlite3's search order, `temp` being empty). The row
grammar folds `x.t` into the table name `x.t` (and `main.t` into `t`) and
gives it the implied alias `t`, so `t.col` and `x.t.col` qualify it and
`EXPLAIN QUERY PLAN` prints `SCAN x.t` as sqlite3 does. A real table whose
name contains a `.` can collide with this listing; none is known. Chosen over a separate
per-database catalog type because every emit site already holds the
table's schema: the database travels with it instead of beside it.

**Storage routes by database.** `CursorFactory` and `SchemaStorage` each
gain one defaulted method, `use_database(db)`, which the VM calls with an
instruction's database index before it opens a cursor or changes the
schema. The default accepts `0` and refuses any other index, so a
downstream implementor (sqlite-rs) keeps compiling and keeps refusing
attached databases; the row engine's implementations switch to that
file's pager. The row engine's `Transaction` hook spans every open file,
so `BEGIN`/`COMMIT`/`ROLLBACK` need no new VM surface.

**Transactions commit per file.** A write to an attached file goes to its
own pager and journal. `COMMIT` (or an autocommit statement's clean
`Halt`) flushes every file the transaction wrote, `main` first, then the
attached ones in index order; `ROLLBACK` or a failed autocommit statement
rolls every pager back. Each file's commit is atomic; the set is not: a
crash between two flushes can leave one file committed and the other not.
This is what sqlite3 itself guarantees for WAL-mode databases (its
super-journal only covers rollback-journal mode).

## Consequences

- Public API: `CrossModeEngine`'s `lookup`/`lookup_path` fields became
  `lookups`, `open_column_tables_sqlite` takes `(name, path)` pairs,
  `TableRef` gained `schema`, `TableSchema` gained `database`. Struct
  literals of `TableSchema` need the field (or `..Default::default()`).
- An atomic multi-file commit (super-journal) is not implemented; it is a
  follow-up if a workload needs it.
- `CREATE TEMP` and tables in `temp` stay unsupported.
- DDL on an attached schema (`CREATE TABLE x.t`, `DROP TABLE x.t`,
  `CREATE INDEX x.i ...`) stays refused as a schema-qualified name; an
  attached file's views are not listed. `INSERT`/`UPDATE`/`DELETE` and
  queries are supported.
