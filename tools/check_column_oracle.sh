#!/usr/bin/env bash
# Local-only oracle-parity aid (ADR 0000 invariant (f), db-core#406): the
# column engine's query results against DuckDB reading the same Parquet
# file -- the same role sqlite3 plays for the SQLite profile, but run by
# hand, never in CI or the weekly assurance workflow (a DuckDB binary is a
# new external dependency with its own supply chain; this charter's whole
# ethos is minimizing exactly that, so it stays off the machine that runs
# untrusted PRs). Run this after a change to `storage::column` or
# `vm::batch`'s aggregate merge path.
#
# For each fixture (three codecs of the same rows: Zstd, Snappy, none) and
# each query pair below, runs the db-core half through
# `examples/column_oracle` and the DuckDB half through the `duckdb` CLI
# reading the file directly, and diffs the two CSV outputs byte for byte.
# The two SQL strings differ only in the cosmetic cast DuckDB needs to print
# a `DOUBLE` at the same fixed precision `column_oracle` uses for `Cell::Real`
# -- the underlying query (and thus what's being checked) is the same.
#
# Usage: tools/check_column_oracle.sh   (needs `duckdb` on PATH -- `brew
# install duckdb` or https://duckdb.org/docs/installation; no CI dependency)
set -euo pipefail

FIXTURES=(
  tests/fixtures/parquet/production.parquet
  tests/fixtures/parquet/production_snappy.parquet
  tests/fixtures/parquet/production_uncompressed.parquet
)

# db_core_sql|duckdb_sql, "__TABLE__" substituted per fixture below.
QUERIES=(
  "SELECT count(*) FROM __TABLE__|SELECT count(*) FROM __TABLE__"
  "SELECT region, count(*) FROM __TABLE__ GROUP BY region ORDER BY region|SELECT region, count(*) FROM __TABLE__ GROUP BY region ORDER BY region"
  "SELECT region, sum(amount) FROM __TABLE__ GROUP BY region ORDER BY region|SELECT region, sum(amount)::DECIMAL(18,2) FROM __TABLE__ GROUP BY region ORDER BY region"
  "SELECT region, avg(amount) FROM __TABLE__ GROUP BY region ORDER BY region|SELECT region, avg(amount)::DECIMAL(18,2) FROM __TABLE__ GROUP BY region ORDER BY region"
  "SELECT region, min(amount), max(amount) FROM __TABLE__ GROUP BY region ORDER BY region|SELECT region, min(amount)::DECIMAL(18,2), max(amount)::DECIMAL(18,2) FROM __TABLE__ GROUP BY region ORDER BY region"
  "SELECT id, amount FROM __TABLE__ WHERE id > 4990 ORDER BY id|SELECT id, amount::DECIMAL(18,2) FROM __TABLE__ WHERE id > 4990 ORDER BY id"
  "SELECT count(DISTINCT region) FROM __TABLE__|SELECT count(DISTINCT region) FROM __TABLE__"
  "SELECT region, count(DISTINCT amount), count(*) FROM __TABLE__ GROUP BY region ORDER BY region|SELECT region, count(DISTINCT amount), count(*) FROM __TABLE__ GROUP BY region ORDER BY region"
  "SELECT region, count(DISTINCT id) FROM __TABLE__ WHERE id > 2500 GROUP BY region ORDER BY region|SELECT region, count(DISTINCT id) FROM __TABLE__ WHERE id > 2500 GROUP BY region ORDER BY region"
  # #623: GROUP BY a real column that an alias shadows, expressions over
  # keys and aggregates, and a select list that is not keys-first.
  "SELECT id - id AS id, count(*) FROM __TABLE__ WHERE id <= 5 GROUP BY id|SELECT id - id AS id, count(*) FROM __TABLE__ WHERE id <= 5 GROUP BY id"
  "SELECT count(*), region FROM __TABLE__ GROUP BY region ORDER BY region|SELECT count(*), region FROM __TABLE__ GROUP BY region ORDER BY region"
  "SELECT region, sum(id) * 2, count(*) + 1 FROM __TABLE__ GROUP BY region ORDER BY region|SELECT region, sum(id) * 2, count(*) + 1 FROM __TABLE__ GROUP BY region ORDER BY region"
  "SELECT region, max(id) - min(id) + count(*) FROM __TABLE__ GROUP BY region ORDER BY region|SELECT region, max(id) - min(id) + count(*) FROM __TABLE__ GROUP BY region ORDER BY region"
  "SELECT count(*) * 2 + sum(id) FROM __TABLE__|SELECT count(*) * 2 + sum(id) FROM __TABLE__"
  # #623 review: a column grouped by its alias, an aggregate aliased like
  # a real column (which wins), COUNT(DISTINCT) with its key left out,
  # LIKE over an aggregate (cast: DuckDB prints a BOOLEAN as true/false).
  "SELECT region AS r, count(*) FROM __TABLE__ GROUP BY r ORDER BY r|SELECT region AS r, count(*) FROM __TABLE__ GROUP BY r ORDER BY r"
  "SELECT sum(id) AS id, count(*) FROM __TABLE__ WHERE id <= 3 GROUP BY id ORDER BY id|SELECT sum(id) AS id, count(*) FROM __TABLE__ WHERE id <= 3 GROUP BY id ORDER BY id"
  "SELECT max(id) AS m, count(DISTINCT amount) FROM __TABLE__ GROUP BY region ORDER BY m|SELECT max(id) AS m, count(DISTINCT amount) FROM __TABLE__ GROUP BY region ORDER BY m"
  "SELECT region, min(region) LIKE 'e%', count(*) FROM __TABLE__ GROUP BY region ORDER BY region|SELECT region, (min(region) LIKE 'e%')::INT, count(*) FROM __TABLE__ GROUP BY region ORDER BY region"
  # #635: integer arithmetic is exact (past 2^53 too) and `/` is DOUBLE.
  "SELECT id, id * 3 - 1, id + id FROM __TABLE__ WHERE id > 4995 ORDER BY id|SELECT id, id * 3 - 1, id + id FROM __TABLE__ WHERE id > 4995 ORDER BY id"
  "SELECT region, sum(id) * 10000000000 + max(id) FROM __TABLE__ GROUP BY region ORDER BY region|SELECT region, sum(id) * 10000000000 + max(id) FROM __TABLE__ GROUP BY region ORDER BY region"
  "SELECT id, id / 4 FROM __TABLE__ WHERE id > 4995 ORDER BY id|SELECT id, (id / 4)::DECIMAL(18,2) FROM __TABLE__ WHERE id > 4995 ORDER BY id"
)

echo "building examples/column_oracle (release)..."
cargo build --release --example column_oracle -q

fail=0
checked=0
for fixture in "${FIXTURES[@]}"; do
  table="$(basename "$fixture" .parquet)"
  for pair in "${QUERIES[@]}"; do
    db_core_q="${pair%%|*}"
    duckdb_q="${pair##*|}"
    db_sql="${db_core_q//__TABLE__/$table}"
    duck_sql="${duckdb_q//__TABLE__/\'$fixture\'}"

    got="$(./target/release/examples/column_oracle "$fixture" "$db_sql")"
    want="$(duckdb -csv -noheader -c "$duck_sql")"

    checked=$((checked + 1))
    if [[ "$got" != "$want" ]]; then
      echo "MISMATCH: $fixture :: $db_sql"
      echo "  db-core: $got"
      echo "  duckdb:  $want"
      fail=1
    fi
  done
done

if [[ "$fail" -ne 0 ]]; then
  echo "check-column-oracle: FAILED"
  exit 1
fi
echo "check-column-oracle: ok -- $checked query/fixture combinations agree with DuckDB"
