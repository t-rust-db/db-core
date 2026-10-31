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
# (read_map_utf8_column's shape check); #801: a list that is not a
# LIST<UTF8> and a struct, neither a map nor a list (read_list_utf8_column's
# shape check, the column engine's leaf columns).
duckdb -c "COPY (SELECT ['x']::VARCHAR[] AS lst, MAP {1: 'a'} AS ik,
    MAP {'a': 1} AS iv, MAP {'a': '1'} AS m, [1]::INTEGER[] AS il,
    {'a': 1} AS st) TO '$DIR/nested_shapes.parquet' (FORMAT PARQUET, COMPRESSION 'snappy');"

# #802: events with a list column -- NULL list, empty list, a NULL element,
# a repeated element, and filter / distinct-count columns beside it.
duckdb -c "COPY (SELECT * FROM (VALUES
    (1, 'a', 'high', 10, ['x', 'y']::VARCHAR[]),
    (2, 'b', 'high', 20, ['y']::VARCHAR[]),
    (3, 'a', 'low', 30, []::VARCHAR[]),
    (4, 'c', 'high', 40, NULL::VARCHAR[]),
    (5, 'a', 'high', 50, ['x', NULL, 'x']::VARCHAR[]),
    (6, 'b', 'low', 60, ['z', 'y']::VARCHAR[])
  ) t(event_id, source, severity, ts, labels) ORDER BY event_id) TO '$DIR/events.parquet' (FORMAT PARQUET, COMPRESSION 'snappy');"

# #803: delimited strings -- NULL, empty, one value, separators at either end
# and doubled, a repeated value -- for string_split and UNNEST over it.
duckdb -c "COPY (SELECT * FROM (VALUES
    (1, 'a,b,c', 'high'),
    (2, 'b', 'low'),
    (3, '', 'high'),
    (4, NULL, 'high'),
    (5, ',a,', 'low'),
    (6, 'a,,b', 'high'),
    (7, 'a,a', 'low')
  ) t(id, csv, severity) ORDER BY id) TO '$DIR/delimited.parquet' (FORMAT PARQUET, COMPRESSION 'snappy');"

# #812: events with a comma-separated labels column and a timestamp, for the
# ranked-labels query (string_split + UNNEST, two count(DISTINCT), min/max
# of the timestamp, a filter on the unnested value) over a file and, with
# the same rows, over an ingest table. An empty string, a NULL, a doubled
# and a trailing separator, a label repeated within an event.
duckdb -c "COPY (SELECT * FROM (VALUES
    (1, 's1', 'high', TIMESTAMP '2026-10-11 10:00:00', 'a,b'),
    (2, 's2', 'critical', TIMESTAMP '2026-10-11 10:05:00', 'b,c,b'),
    (3, 's1', 'low', TIMESTAMP '2026-10-11 10:10:00', 'a,b'),
    (4, 's3', 'high', TIMESTAMP '2026-10-11 11:00:00', ''),
    (5, 's2', 'high', TIMESTAMP '2026-10-11 11:30:00', NULL),
    (6, 's1', 'critical', TIMESTAMP '2026-10-12 09:00:00', 'b,,a,'),
    (7, 's2', 'high', TIMESTAMP '2026-10-12 09:30:00', 'c'),
    (8, 's3', 'info', TIMESTAMP '2026-10-12 10:00:00', 'a')
  ) t(event_id, source, severity, ts, labels) ORDER BY event_id) TO '$DIR/ranked_events.parquet' (FORMAT PARQUET, COMPRESSION 'snappy');"

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

# #680: the column sqllogictest suite's NULL-heavy fixture -- 10000 rows,
# NULLs in every column but id, one column per type the column engine
# reads (integer, double, text, boolean, decimal, timestamp), five row
# groups, and a skewed group key (80% / 10% / 9% / 1% NULL). Every value
# is a function of id, so the file is deterministic.
duckdb -c "COPY (SELECT
    i AS id,
    CASE WHEN i % 7 = 0 THEN NULL ELSE (i * 37) % 1000 - 500 END::INTEGER AS n,
    CASE WHEN i % 11 = 0 THEN NULL ELSE ((i * 13) % 10007) / 8.0 END::DOUBLE AS f,
    CASE WHEN i % 13 = 0 THEN NULL WHEN i % 13 = 1 THEN ''
         ELSE ['alpha', 'Beta', 'GAMMA', 'delta'][1 + i % 4] || (i % 50)::VARCHAR END AS s,
    CASE WHEN i % 5 = 0 THEN NULL ELSE i % 3 = 0 END AS b,
    CASE WHEN i % 17 = 0 THEN NULL ELSE ((i * 7919) % 100000 - 50000) / 100 END::DECIMAL(9,2) AS d,
    CASE WHEN i % 19 = 0 THEN NULL ELSE TIMESTAMP '2026-01-01 00:00:00' + to_minutes(i * 97) END AS ts,
    CASE WHEN i % 10 < 8 THEN 'big' WHEN i % 10 = 8 THEN 'small' WHEN i % 100 <> 99 THEN 'tiny' END AS grp
  FROM range(1, 10001) t(i) ORDER BY id)
  TO '$DIR/typed_nulls.parquet' (FORMAT PARQUET, COMPRESSION 'snappy', ROW_GROUP_SIZE 2048);"

# #845: three BIGINT ids around 2^62 (past f64's 2^53 exact range) that
# differ by 1; big2 equals big on the even ids and exceeds it by 1 on the odd.
duckdb -c "COPY (SELECT i AS id, 4611686018427387904 + i AS big, 4611686018427387904 + i + (i % 2) AS big2
  FROM range(0, 4) t(i) ORDER BY id)
  TO '$DIR/big_ints.parquet' (FORMAT PARQUET, COMPRESSION 'snappy');"

# #868: BLOB columns -- a 16-byte key from a small set (some repeated), a
# 32-byte digest NULL on every third row, and an empty blob; three row groups
# (DuckDB writes whole 2048-row vectors) so a blob min/max differs per group.
# seq is a 4-byte big-endian id from 0xFFFF0000, first byte above 0x7F, so
# its row groups hold disjoint ranges and a signed byte compare would misorder
# them. Values are a function of id.
duckdb -c "COPY (SELECT
    i AS id,
    CASE WHEN i % 20 = 0 THEN ''::BLOB
         ELSE unhex(repeat(printf('%02X', (i % 5) * 51), 16)) END AS k,
    CASE WHEN i % 3 = 0 THEN NULL ELSE unhex(repeat(printf('%02X', i % 256), 32)) END AS d,
    unhex(printf('%08X', i + 4294901760)) AS seq
  FROM range(0, 5000) t(i) ORDER BY id)
  TO '$DIR/blobs.parquet' (FORMAT PARQUET, COMPRESSION 'snappy', ROW_GROUP_SIZE 2048);"

echo "generated $DIR/production_snappy.parquet, $DIR/production_uncompressed.parquet, $DIR/nested_map.parquet, $DIR/nested_shapes.parquet, $DIR/events.parquet, $DIR/delimited.parquet, $DIR/ranked_events.parquet, $DIR/null_types.parquet, $DIR/typed_nulls.parquet, $DIR/big_ints.parquet, $DIR/blobs.parquet"
