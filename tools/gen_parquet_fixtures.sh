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

# #621: nested fields that are not a MAP<UTF8, UTF8>, and one that is
# (read_map_utf8_column's shape check).
duckdb -c "COPY (SELECT ['x']::VARCHAR[] AS lst, MAP {1: 'a'} AS ik,
    MAP {'a': 1} AS iv, MAP {'a': '1'} AS m) TO '$DIR/nested_shapes.parquet' (FORMAT PARQUET, COMPRESSION 'snappy');"

# #679: NULLs in every typed column, mixed-case and empty text, a decimal
# and a timestamp -- the shapes the column engine answered wrongly
# (three-valued AND/OR, quoted-value comparisons, timestamps, LIKE case).
duckdb -c "COPY (SELECT * FROM (VALUES
    (1, 'a', 10, 1.5::DECIMAL(2,1), TIMESTAMP '2026-01-01 10:00:00', 'Error'),
    (2, NULL, NULL, NULL, TIMESTAMP '2026-01-02 11:30:00', 'error'),
    (3, 'b', -7, 2.5::DECIMAL(2,1), NULL, 'WARN'),
    (4, 'B', 3, NULL, TIMESTAMP '2026-01-02 23:59:59', '%_x'),
    (5, '', 0, 0.0::DECIMAL(2,1), TIMESTAMP '2026-01-03 00:00:00', NULL)
  ) t(id, s, n, d, ts, lvl) ORDER BY id) TO '$DIR/null_types.parquet' (FORMAT PARQUET, COMPRESSION 'snappy');"

echo "generated $DIR/production_snappy.parquet, $DIR/production_uncompressed.parquet, $DIR/nested_map.parquet, $DIR/nested_shapes.parquet, $DIR/null_types.parquet"
