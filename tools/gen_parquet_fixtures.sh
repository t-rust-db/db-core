#!/usr/bin/env bash
# Regenerates the codec-variant Parquet fixtures for the column oracle gate
# (db-core#406): the same rows as tests/fixtures/parquet/production.parquet
# (which is itself Zstd-compressed), recompressed with the other codecs
# storage/column/parquet/compression.rs decodes -- Snappy and none -- so
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

echo "generated $DIR/production_snappy.parquet, $DIR/production_uncompressed.parquet"
