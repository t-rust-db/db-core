#!/usr/bin/env bash
# Regenerates the codec-variant Parquet fixtures for the column oracle gate
# (db-core#406): the same rows as tests/fixtures/parquet/production.parquet
# (which is itself Zstd-compressed), recompressed with the other codecs
# storage/parquet/compression.rs decodes -- Snappy and none -- so
# tools/check_column_oracle.sh exercises more than one compression path.
#
# Needs the `duckdb` CLI on PATH (local dev tool only, no CI dependency --
# see tools/check_column_oracle.sh). Run from the repo root; only needs
# re-running if production.parquet's rows change.
set -euo pipefail

SRC="tests/fixtures/parquet/production.parquet"
DIR="tests/fixtures/parquet"

duckdb -c "COPY (SELECT * FROM '$SRC') TO '$DIR/production_snappy.parquet' (FORMAT PARQUET, COMPRESSION 'snappy');"
duckdb -c "COPY (SELECT * FROM '$SRC') TO '$DIR/production_uncompressed.parquet' (FORMAT PARQUET, COMPRESSION 'uncompressed');"

# #621: a MAP between two flat columns, as DuckDB writes it (dictionary-
# encoded leaves): two entries, a NULL map, an empty map, a NULL value, a
# non-ASCII key. The column after the map checks leaf indexing.
duckdb -c "COPY (SELECT * FROM (VALUES
    (0, MAP {'User-Agent': 'curl/8', 'Accept': '*/*'}, 0),
    (1, NULL, 10),
    (2, MAP {}::MAP(VARCHAR, VARCHAR), 20),
    (3, MAP {'k': NULL::VARCHAR}, 30),
    (4, MAP {'héader': 'välue', 'k': 'v'}, 40)
  ) t(id, h, x) ORDER BY id) TO '$DIR/nested_map.parquet' (FORMAT PARQUET, COMPRESSION 'snappy');"

echo "generated $DIR/production_snappy.parquet, $DIR/production_uncompressed.parquet, $DIR/nested_map.parquet"
