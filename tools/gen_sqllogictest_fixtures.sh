#!/usr/bin/env bash
# Regenerates the Parquet fixtures behind the grown column sqllogictest
# suite (db-core#1014): NULL-heavy groups, integer/float/decimal
# boundaries, timestamp edges and a many-row-group file. Kept apart from
# tools/gen_parquet_fixtures.sh so regenerating these never rewrites the
# codec-variant fixtures the column oracle gate reads.
#
# Needs the `duckdb` CLI on PATH (local dev tool only; CI reads the
# committed files). Run from the repo root. The rows are deterministic
# (no random(), no now()), so a re-run with the same DuckDB is a no-op diff.
set -euo pipefail

DIR="tests/fixtures/parquet"

# null_groups: 240 rows over six groups. `all_null` has NULL in every value
# column (an all-NULL group), `one` is a single row, `zeros` holds zeros
# (a group whose min = max = 0), and the NULL group is a group of its own.
duckdb -c "COPY (
  SELECT id::INTEGER AS id, g, k, v, x, d, s, b, ts FROM (
    SELECT id,
      CASE WHEN id < 80 THEN 'mixed' WHEN id < 120 THEN 'all_null'
           WHEN id = 120 THEN 'one' WHEN id < 151 THEN 'zeros'
           WHEN id < 191 THEN 'neg' ELSE NULL END AS g,
      id % 7 AS k,
      CASE WHEN id < 80 THEN CASE WHEN id % 4 = 0 THEN NULL ELSE id * 3 - 100 END
           WHEN id < 120 THEN NULL WHEN id = 120 THEN 42 WHEN id < 151 THEN 0
           WHEN id < 191 THEN -(id - 150) ELSE id % 13 END AS v,
      CASE WHEN id < 80 THEN CASE WHEN id % 5 = 0 THEN NULL ELSE id / 8.0 END
           WHEN id < 120 THEN NULL WHEN id = 120 THEN 2.5 WHEN id < 151 THEN 0.0
           WHEN id < 191 THEN -(id - 150) / 4.0 ELSE (id % 9) * 1.25 END AS x,
      CASE WHEN id < 80 THEN CASE WHEN id % 6 = 0 THEN NULL ELSE (id * 7 % 1000) / 100.0 END
           WHEN id < 120 THEN NULL WHEN id = 120 THEN 9.99 WHEN id < 151 THEN 0.00
           WHEN id < 191 THEN -(id - 150) * 1.11 ELSE (id % 17) * 2.5 END::DECIMAL(10,2) AS d,
      CASE WHEN id < 80 THEN CASE WHEN id % 3 = 0 THEN NULL ELSE 'm' || id::VARCHAR END
           WHEN id < 120 THEN NULL WHEN id = 120 THEN 'solo' WHEN id < 151 THEN ''
           WHEN id < 191 THEN 'Neg' || (id % 5)::VARCHAR ELSE 'n' || (id % 4)::VARCHAR END AS s,
      CASE WHEN id < 80 THEN CASE WHEN id % 8 = 0 THEN NULL ELSE id % 2 = 0 END
           WHEN id < 120 THEN NULL WHEN id = 120 THEN true WHEN id < 151 THEN false
           WHEN id < 191 THEN id % 3 = 0 ELSE id % 5 < 2 END AS b,
      CASE WHEN id % 9 = 4 THEN NULL
           ELSE TIMESTAMP '2026-03-01 00:00:00' + INTERVAL (id * 37) MINUTE END AS ts
    FROM range(240) t(id)
  ) ORDER BY id
) TO '$DIR/null_groups.parquet' (FORMAT PARQUET, COMPRESSION 'snappy');"

# int_bounds: the extremes of every width the engine reads, beside zero,
# +-1 and a NULL, plus doubles at the edges of the format (signed zero,
# 2^53, 1e+-300, the classic 0.1 + 0.2) and DECIMAL(18,3) extremes.
duckdb -c "COPY (SELECT * FROM (VALUES
    (1, (-128)::TINYINT, (-32768)::SMALLINT, (-2147483648)::INTEGER, (-9223372036854775808)::BIGINT, (-1e300)::DOUBLE, (-999999999999999.999)::DECIMAL(18,3)),
    (2, -127, -32767, -2147483647, -9223372036854775807, -9007199254740992.0, -0.001),
    (3, -1, -1, -1, -1, -0.1, -1.000),
    (4, 0, 0, 0, 0, 0.0, 0.000),
    (5, 1, 1, 1, 1, 0.1, 1.000),
    (6, 126, 32766, 2147483646, 9223372036854775806, 0.30000000000000004, 0.001),
    (7, 127, 32767, 2147483647, 9223372036854775807, 9007199254740992.0, 999999999999999.999),
    (8, NULL, NULL, NULL, NULL, NULL, NULL),
    (9, 10, 100, 1000000, 1000000000000, 1e-300, 123456789012.345),
    (10, -10, -100, -1000000, -1000000000000, 1e300, -123456789012.345),
    (11, 0, 0, 0, 0, -0.0, 0.000),
    (12, 64, 16384, 1073741824, 4611686018427387904, 1.0 / 3.0, 333333333333.333)
  ) t(id, i8, i16, i32, i64, f, d) ORDER BY id) TO '$DIR/int_bounds.parquet' (FORMAT PARQUET, COMPRESSION 'snappy');"

# ts_edges: the epoch, either side of it, the 2038 rollover, leap days (and
# the century years that are not leap years), year 1 and 9999, microsecond
# precision, midnight, and a NULL.
duckdb -c "COPY (SELECT * FROM (VALUES
    (1, TIMESTAMP '1970-01-01 00:00:00'),
    (2, TIMESTAMP '1969-12-31 23:59:59'),
    (3, TIMESTAMP '1969-12-31 23:59:59.999999'),
    (4, TIMESTAMP '2038-01-19 03:14:07'),
    (5, TIMESTAMP '2038-01-19 03:14:08'),
    (6, TIMESTAMP '2000-02-29 12:00:00'),
    (7, TIMESTAMP '1900-02-28 23:59:59'),
    (8, TIMESTAMP '1900-03-01 00:00:00'),
    (9, TIMESTAMP '0001-01-01 00:00:00'),
    (10, TIMESTAMP '9999-12-31 23:59:59.999999'),
    (11, TIMESTAMP '2024-02-29 23:59:59.999999'),
    (12, TIMESTAMP '2026-12-31 00:00:00'),
    (13, NULL),
    (14, TIMESTAMP '1600-01-01 00:00:00'),
    (15, TIMESTAMP '2100-02-28 00:00:01'),
    (16, TIMESTAMP '2100-03-01 00:00:00'),
    (17, TIMESTAMP '2026-01-01 00:00:00.000001'),
    (18, TIMESTAMP '2026-01-01 00:00:00.000002'),
    (19, TIMESTAMP '2026-06-15 12:30:45.123456'),
    (20, TIMESTAMP '2026-06-15 12:30:45.123456')
  ) t(id, ts) ORDER BY id) TO '$DIR/ts_edges.parquet' (FORMAT PARQUET, COMPRESSION 'snappy');"

# multi_rg: 30,000 rows in 15 row groups (DuckDB rounds ROW_GROUP_SIZE up to
# a multiple of its 2,048-row vector). `blk` is id / 1000, so a filter on it
# prunes whole groups without lining up with the group boundaries; `k` has NULLs and
# varies inside a group; `g` clusters by group for some blocks.
duckdb -c "COPY (
  SELECT id::BIGINT AS id, (id // 1000)::INTEGER AS blk,
    CASE WHEN id % 41 = 0 THEN NULL
         WHEN (id // 1000) % 3 = 0 THEN 'g' || ((id // 1000) % 5)::VARCHAR
         ELSE 'g' || (id % 5)::VARCHAR END AS g,
    CASE WHEN id % 7 = 0 THEN NULL ELSE (id * 31) % 1009 - 500 END::INTEGER AS k,
    CASE WHEN id % 13 = 0 THEN NULL ELSE ((id * 17) % 2003) / 8.0 END AS x,
    CASE WHEN id % 11 = 0 THEN NULL ELSE ((id * 29) % 99991) / 100.0 END::DECIMAL(9,2) AS d,
    CASE WHEN id % 19 = 0 THEN NULL ELSE 's' || (id % 97)::VARCHAR END AS s,
    TIMESTAMP '2026-01-01 00:00:00' + INTERVAL (id * 11) MINUTE AS ts
  FROM range(30000) t(id) ORDER BY id
) TO '$DIR/multi_rg.parquet' (FORMAT PARQUET, COMPRESSION 'snappy', ROW_GROUP_SIZE 1000);"
