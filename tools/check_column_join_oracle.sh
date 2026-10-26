#!/usr/bin/env bash
# Local-only oracle-parity aid (#598), the join sibling of
# check_column_oracle.sh: a Parquet table joined to SQLite lookup tables
# through `CrossModeEngine::open_column_sqlite` against DuckDB reading the
# same Parquet file with the same SQLite file attached. The same queries
# then run over a table directory (#772, `open_table_sqlite`), against
# DuckDB reading its files: the directory is flushed first, as DuckDB
# cannot read a WAL, so rows in live pages are covered by the unit tests. Run by hand after a
# change to `engine::resolve` or its `prune` module; never in CI (a DuckDB
# binary is a new external dependency, see check_column_oracle.sh).
#
# Each query is written once with two placeholders: __P__ is the Parquet
# table (`production` for db-core, the file `AS production` for DuckDB) and
# __ids__/__regions__ are the lookup tables (bare for db-core, `lk.<name>` for
# DuckDB). Every query has a total ORDER BY, or is an aggregate over a
# non-NULL key, since DuckDB and SQLite order NULLs differently.
#
# Usage: tools/check_column_join_oracle.sh   (needs `duckdb` and `sqlite3`
# on PATH; DuckDB installs its `sqlite` extension on first use, which needs
# network once)
set -euo pipefail

FIXTURE=tests/fixtures/parquet/production.parquet

# production: id 1..=5000, region north/south/east/west (1250 each).
# `ids` matches only some of them (and has rows production never has);
# `regions` has no `west`, and `west` rows must surface as NULL on a left join.
SEED="
CREATE TABLE ids(id INTEGER PRIMARY KEY, label TEXT, weight INTEGER);
INSERT INTO ids VALUES (1,'one',10),(2,'two',20),(3,'three',30),(10,'ten',5),(4999,'last',7),(6000,'nowhere',1);
CREATE TABLE regions(region TEXT PRIMARY KEY, zone TEXT);
INSERT INTO regions VALUES ('north','A'),('south','A'),('east','B');
-- a two-column key: ids 1, 2 and 10 agree with production's region too
CREATE TABLE pairs(id INTEGER, region TEXT, tag TEXT);
INSERT INTO pairs VALUES (1,'west','w1'),(2,'north','n2'),(3,'east','e3'),(4,'south','s4'),(10,'north','n10');
"

J_IDS="__P__ JOIN __ids__ ON production.id = ids.id"
L_IDS="__P__ LEFT JOIN __ids__ ON production.id = ids.id"
J_REG="__P__ JOIN __regions__ ON production.region = regions.region"
L_REG="__P__ LEFT JOIN __regions__ ON production.region = regions.region"

QUERIES=(
  # inner and left join on an integer key
  "SELECT production.id, ids.label FROM $J_IDS ORDER BY production.id"
  "SELECT count(*) FROM $J_IDS"
  "SELECT count(*) FROM $L_IDS"
  "SELECT production.id, ids.label FROM $L_IDS ORDER BY production.id LIMIT 6"
  "SELECT count(*) FROM $L_IDS WHERE ids.label IS NULL"
  "SELECT count(ids.label), count(*) FROM $L_IDS"
  # the lookup side written first still joins inner
  "SELECT production.id, ids.label FROM __ids__ JOIN __P__ ON ids.id = production.id ORDER BY production.id"
  # filters on the lookup table (the key-set pruning), inner and left
  "SELECT production.id FROM $J_IDS WHERE ids.label = 'two'"
  "SELECT production.id FROM $J_IDS WHERE ids.label IN ('one', 'last') ORDER BY production.id"
  "SELECT production.id FROM $J_IDS WHERE ids.label = 'one' OR ids.label = 'ten' ORDER BY production.id"
  "SELECT production.id FROM $J_IDS WHERE ids.weight > 8 AND ids.label <> 'one' ORDER BY production.id"
  "SELECT production.id FROM $J_IDS WHERE ids.id BETWEEN 2 AND 10 ORDER BY production.id"
  "SELECT count(*) FROM $J_IDS WHERE ids.label = 'nowhere'"
  "SELECT count(*) FROM $J_IDS WHERE ids.label = 'absent'"
  "SELECT count(*) FROM $J_IDS WHERE ids.label IS NULL"
  "SELECT production.id FROM $L_IDS WHERE ids.label = 'two'"
  "SELECT count(*) FROM $L_IDS WHERE ids.label IS NULL OR ids.label = 'two'"
  "SELECT count(*) FROM $J_IDS WHERE ids.label = 'one' OR production.id = 3"
  # a filter on the probe side next to the join
  "SELECT production.id, ids.label FROM $J_IDS WHERE production.id > 2 ORDER BY production.id"
  # a text join key
  "SELECT count(*) FROM $J_REG"
  "SELECT regions.zone, count(*), min(production.id) FROM $J_REG GROUP BY regions.zone ORDER BY regions.zone"
  "SELECT count(*) FROM $J_REG WHERE regions.zone = 'B'"
  "SELECT count(*) FROM $L_REG WHERE regions.zone IS NULL"
  "SELECT production.region, count(regions.zone), count(*) FROM $L_REG GROUP BY production.region ORDER BY production.region"
  # JOIN ... USING (k): one key column, the FROM table's value for an
  # unqualified k, inner and left (#771)
  "SELECT production.id, ids.label FROM __P__ JOIN __ids__ USING (id) ORDER BY production.id"
  "SELECT id, ids.label FROM __P__ LEFT JOIN __ids__ USING (id) ORDER BY id LIMIT 6"
  "SELECT count(*) FROM __P__ LEFT JOIN __ids__ USING (id) WHERE ids.label IS NULL"
  "SELECT production.id FROM __P__ JOIN __ids__ USING (id) WHERE ids.label IN ('one', 'last') ORDER BY production.id"
  "SELECT region, count(*) FROM __P__ JOIN __ids__ USING (id) GROUP BY region ORDER BY region"
  "SELECT id, count(*) FROM __P__ JOIN __ids__ USING (id) WHERE id > 1 GROUP BY id ORDER BY id"
  "SELECT id FROM __P__ LEFT JOIN __ids__ USING (id) WHERE id < 5 AND ids.label IS NULL ORDER BY id"
  # an unqualified column is the one table's that has it, either table
  # written first; ORDER BY names the output column
  "SELECT label, region FROM __P__ JOIN __ids__ ON production.id = ids.id WHERE label = 'two'"
  "SELECT label, region FROM __ids__ JOIN __P__ ON ids.id = production.id WHERE label = 'two'"
  "SELECT label, count(*) FROM __P__ JOIN __ids__ ON production.id = ids.id GROUP BY label ORDER BY label"
  "SELECT production.id FROM __P__ JOIN __ids__ ON production.id = ids.id ORDER BY id DESC"
  "SELECT region, zone, count(*) FROM __P__ JOIN __regions__ USING (region) GROUP BY region, zone ORDER BY region"
  # the ON operands in either order
  "SELECT production.id, ids.label FROM __P__ JOIN __ids__ ON ids.id = production.id ORDER BY production.id"
  "SELECT production.id, ids.label FROM __P__ LEFT JOIN __ids__ ON ids.id = production.id ORDER BY production.id LIMIT 6"
  "SELECT production.id, ids.label FROM __ids__ JOIN __P__ ON production.id = ids.id ORDER BY production.id"
  "SELECT production.id, ids.label FROM __ids__ JOIN __P__ USING (id) ORDER BY production.id"
  # SELECT *: every column of each table in the order written, the USING
  # key once
  "SELECT * FROM __P__ JOIN __ids__ USING (id) ORDER BY id"
  "SELECT * FROM __P__ JOIN __ids__ ON production.id = ids.id ORDER BY production.id"
  "SELECT * FROM __P__ JOIN __regions__ ON production.region = regions.region WHERE production.id < 6 ORDER BY production.id"
  # USING (a, b): hash on a, filter on b, inner joins
  "SELECT production.id, pairs.tag FROM __P__ JOIN __pairs__ USING (id, region) ORDER BY production.id"
  "SELECT * FROM __P__ JOIN __pairs__ USING (id, region) ORDER BY id"
  "SELECT count(*) FROM __P__ JOIN __pairs__ USING (id, region) WHERE pairs.tag LIKE 'n%'"
  "SELECT region, count(*) FROM __P__ JOIN __pairs__ USING (id, region) GROUP BY region ORDER BY region"
  # t.*: one table's columns, the USING key included
  "SELECT ids.* FROM __P__ JOIN __ids__ USING (id) ORDER BY ids.id"
  "SELECT production.*, ids.label FROM __P__ JOIN __ids__ ON production.id = ids.id ORDER BY production.id"
  "SELECT regions.* FROM __P__ JOIN __regions__ USING (region) WHERE production.id < 5 ORDER BY production.id"
  # a star join over both lookup tables
  "SELECT production.id, ids.label, regions.zone FROM __P__ JOIN __ids__ ON production.id = ids.id JOIN __regions__ ON production.region = regions.region ORDER BY production.id"
  "SELECT production.id, ids.label, regions.zone FROM __P__ JOIN __ids__ ON production.id = ids.id JOIN __regions__ ON production.region = regions.region WHERE regions.zone = 'A' AND ids.weight >= 10 ORDER BY production.id"
  "SELECT regions.zone, count(*) FROM __P__ JOIN __ids__ ON production.id = ids.id JOIN __regions__ ON production.region = regions.region GROUP BY regions.zone ORDER BY regions.zone"
)

# Queries both engines must refuse: an unqualified column that two tables
# have is ambiguous (unless it is the USING key), and so is an unknown one.
# (`LEFT JOIN ... USING (a, b)` is valid in DuckDB; db-core refuses it by
# design, #771: the extra keys filter after the join, which an outer join
# cannot do.)
ERROR_QUERIES=(
  "SELECT id FROM __P__ JOIN __ids__ ON production.id = ids.id"
  "SELECT production.id FROM __P__ JOIN __ids__ ON production.id = ids.id WHERE id = 2"
  "SELECT region FROM __P__ JOIN __ids__ ON production.id = ids.id ORDER BY id"
  "SELECT nope FROM __P__ JOIN __ids__ USING (id)"
)

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT
lookup="$workdir/lookup.db"
sqlite3 "$lookup" "$SEED"

echo "building examples/column_join_oracle (release)..."
cargo build --release --example column_join_oracle -q

fail=0
checked=0
for template in "${QUERIES[@]}"; do
  db_sql="${template//__P__/production}"
  db_sql="${db_sql//__ids__/ids}"
  db_sql="${db_sql//__regions__/regions}"
  db_sql="${db_sql//__pairs__/pairs}"
  duck_sql="${template//__P__/\'$FIXTURE\' AS production}"
  duck_sql="${duck_sql//__ids__/lk.ids}"
  duck_sql="${duck_sql//__regions__/lk.regions}"
  duck_sql="${duck_sql//__pairs__/lk.pairs}"

  got="$(./target/release/examples/column_join_oracle "$FIXTURE" "$lookup" "$db_sql")"
  want="$(duckdb -csv -noheader -nullvalue '' -c "INSTALL sqlite; LOAD sqlite; ATTACH '$lookup' AS lk (TYPE sqlite); $duck_sql" | grep -v '^$' || true)"

  checked=$((checked + 1))
  if [[ "$got" != "$want" ]]; then
    echo "MISMATCH: $db_sql"
    echo "  db-core: $got"
    echo "  duckdb:  $want"
    fail=1
  fi
done

for template in "${ERROR_QUERIES[@]}"; do
  db_sql="${template//__P__/production}"
  db_sql="${db_sql//__ids__/ids}"
  db_sql="${db_sql//__regions__/regions}"
  db_sql="${db_sql//__pairs__/pairs}"
  duck_sql="${template//__P__/\'$FIXTURE\' AS production}"
  duck_sql="${duck_sql//__ids__/lk.ids}"
  duck_sql="${duck_sql//__regions__/lk.regions}"
  duck_sql="${duck_sql//__pairs__/lk.pairs}"

  checked=$((checked + 1))
  if ./target/release/examples/column_join_oracle "$FIXTURE" "$lookup" "$db_sql" >/dev/null 2>&1; then
    echo "NOT REFUSED by db-core: $db_sql"
    fail=1
  fi
  if duckdb -csv -noheader -c "INSTALL sqlite; LOAD sqlite; ATTACH '$lookup' AS lk (TYPE sqlite); $duck_sql" >/dev/null 2>&1; then
    echo "NOT REFUSED by duckdb (the expectation is wrong): $db_sql"
    fail=1
  fi
done

# The same joins over a table directory named `production` (#772): five
# archive files of one row group each, so the key-set pruning skips whole
# files. `SELECT *` and `production.*` list the Parquet fixture's columns,
# which the directory (id, region) does not have.
tabledir="$workdir/production"
./target/release/examples/column_join_oracle --seed-dir "$tabledir"
dir_files="$tabledir/**/*.parquet"
for template in "${QUERIES[@]}"; do
  case "$template" in *"SELECT *"*|*"production.*"*) continue ;; esac
  db_sql="${template//__P__/production}"
  db_sql="${db_sql//__ids__/ids}"
  db_sql="${db_sql//__regions__/regions}"
  db_sql="${db_sql//__pairs__/pairs}"
  duck_sql="${template//__P__/read_parquet(\'$dir_files\') AS production}"
  duck_sql="${duck_sql//__ids__/lk.ids}"
  duck_sql="${duck_sql//__regions__/lk.regions}"
  duck_sql="${duck_sql//__pairs__/lk.pairs}"

  got="$(./target/release/examples/column_join_oracle "$tabledir" "$lookup" "$db_sql")"
  want="$(duckdb -csv -noheader -nullvalue '' -c "INSTALL sqlite; LOAD sqlite; ATTACH '$lookup' AS lk (TYPE sqlite); $duck_sql" | grep -v '^$' || true)"

  checked=$((checked + 1))
  if [[ "$got" != "$want" ]]; then
    echo "MISMATCH (table directory): $db_sql"
    echo "  db-core: $got"
    echo "  duckdb:  $want"
    fail=1
  fi
done

if [[ "$fail" -ne 0 ]]; then
  echo "check-column-join-oracle: FAILED"
  exit 1
fi
echo "check-column-join-oracle: ok -- $checked queries agree with DuckDB"
