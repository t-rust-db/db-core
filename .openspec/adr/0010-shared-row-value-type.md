# ADR 0010: One row `Value` type, defined in `db-core::value`, consumed by `db-storage`

## Status

Accepted.

## Decision

`db_core::value` is a feature-free, dependency-free leaf module holding
`Value`, `TextEncoding`, `Collation`, `compare_text` and `format_real`.
It is the one row value type in the family:

- `vm::row::value` re-exports it; the VM computes over it.
- `db_storage::row::record` and `row::format` re-export it; the b-tree
  decodes into it. `db-storage` depends on `db-core` with
  `default-features = false`, which pulls in nothing but these leaf
  modules.

The dependency direction is `db-storage -> db-core`, never the reverse
(ADR 0006). Leaf types every layer shares live in `db-core`, and storage
consumes them.

## Rationale

Two structurally identical enums on either side of the storage boundary
would force every cursor adapter to convert each cell and would let
collation-aware comparison drift between two copies. A third
micro-crate for a few hundred lines was considered and not needed:
`default-features = false` already isolates the leaf modules.

## Consequences

- A `db-storage` cursor passes cells through untouched.
- `db-storage` releases track a `db-core` tag; bumping it is a reviewed
  dependency update.
- A `db-storage` dependency inside `db-core` would be an ADR 0006
  violation, not an oversight.
