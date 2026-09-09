# ADR 0016: Absorb db-storage as a workspace member

## Status

Accepted (#287)

## Decision

db-storage is merged into this repo as a Cargo workspace member
(`db-storage/`), history preserved via `git subtree`. db-storage's
`db-core` dependency is a path dependency (`{ path = ".." }`) instead of
a git/tag dependency. One repo, one Cargo.lock, one compiler pass.

## Structure

- Root `Cargo.toml`: `[workspace] members = [".", "db-storage"]`, root
  package remains `db-core`.
- `db-storage/`: unchanged crate contents, own `Cargo.toml`, `Makefile`,
  `clippy.toml`, `deny.toml` — its own gates still run from within that
  directory.
- Root `Makefile`: `test`/`test-lib`/`test-spike` scoped to `-p db-core`
  explicitly. `test-storage` runs `db-storage`'s suite
  (`$(MAKE) -C db-storage test`). `ci` runs both crates' lint and test
  targets.

## Consequences

- `cargo build/test/clippy --workspace` covers both crates in one pass.
- The standalone `t-rust-db/db-storage` repo is superseded. Consumers
  (sqlite-rs, column-rs, trigrep) repoint their `db-storage` git
  dependency at this repo's `db-storage/` subdirectory
  (`package = "db-storage"`, path `db-storage`) — tracked as separate
  tickets in each consumer repo.
- ADR-0040 (sqlite-rs)'s "first-party crates pinned by tag, independent
  repos" convention no longer applies to the db-core/db-storage pair;
  it still holds for every other first-party dependency.
