"""Generates the matrix files of the column sqllogictest suite (db-core#1014).

Writes tests/sqllogictest/column/matrix_*.test with `query` records whose
`----` blocks are empty: `make refresh-column-sqllogictest` then fills every
expectation from DuckDB, so no result in these files is typed by hand.

    python3 tools/gen_column_sqllogictest.py tests/sqllogictest/column
    make refresh-column-sqllogictest

Re-running regenerates the files byte for byte but empties the expectations
again, so refresh afterwards. Only shapes the column engine accepts are
generated (what it refuses is pinned by hand in errors_1014.test). A wrong
answer found by the matrix is listed in `WRONG` below with its issue number:
the record is then emitted under `skipif db-core`, with DuckDB's answer kept.

Fixtures (tests/fixtures/parquet/): typed_nulls, production, null_types
(older), null_groups, int_bounds, ts_edges, multi_rg (tools/
gen_sqllogictest_fixtures.sh). A TIMESTAMP column is never selected raw: the
column engine prints it as integer microseconds until the timestamp type
lands (#678 milestone 3), which types.test already records.
"""

import pathlib
import sys

# sql -> (issue, one-line note): the column engine's answer differs from
# DuckDB's (#1016-#1020, found by this matrix).
WRONG: dict[str, tuple[str, str]] = {
    "SELECT id, -d, abs(d) FROM multi_rg ORDER BY id LIMIT 12": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, -d, abs(d) FROM null_groups ORDER BY id LIMIT 12": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, -d, abs(d) FROM typed_nulls ORDER BY id LIMIT 12": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, -d, abs(d), d * -1 FROM multi_rg ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, -d, abs(d), d * -1 FROM null_groups ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, -d, abs(d), d * -1 FROM typed_nulls ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, abs(d) + abs(-1) FROM multi_rg ORDER BY id LIMIT 12": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, abs(d) + abs(-1) FROM null_groups ORDER BY id LIMIT 12": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, abs(d) + abs(-1) FROM typed_nulls ORDER BY id LIMIT 12": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, abs(d) FROM int_bounds WHERE id NOT IN (1) ORDER BY id": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, d * -0.01 FROM multi_rg ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, d * -0.01 FROM null_groups ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, d * -0.01 FROM typed_nulls ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, d * 0.5 FROM multi_rg ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, d * 0.5 FROM null_groups ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, d * 0.5 FROM typed_nulls ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, d * 1.25 FROM multi_rg ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, d * 1.25 FROM null_groups ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, d * 1.25 FROM typed_nulls ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, d + -0.01 FROM multi_rg ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, d + -0.01 FROM null_groups ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, d + -0.01 FROM typed_nulls ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, d + 0.001, d - 0.001 FROM int_bounds WHERE id IN (2, 3, 4, 5, 6, 9, 10, 11, 12) ORDER BY id": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, d + 0.5 FROM multi_rg ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, d + 0.5 FROM null_groups ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, d + 0.5 FROM typed_nulls ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, d + 1.25 FROM multi_rg ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, d + 1.25 FROM null_groups ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, d + 1.25 FROM typed_nulls ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, d - -0.01 FROM multi_rg ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, d - -0.01 FROM null_groups ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, d - -0.01 FROM typed_nulls ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, d - 0.5 FROM multi_rg ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, d - 0.5 FROM null_groups ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, d - 0.5 FROM typed_nulls ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, d - 1.25 FROM multi_rg ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, d - 1.25 FROM null_groups ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, d - 1.25 FROM typed_nulls ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, round(d) FROM multi_rg ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, round(d) FROM null_groups ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, round(d) FROM typed_nulls ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, round(d, 0) FROM multi_rg ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, round(d, 0) FROM null_groups ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, round(d, 0) FROM typed_nulls ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, round(d, 2) FROM multi_rg ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, round(d, 2) FROM null_groups ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, round(d, 2) FROM typed_nulls ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, round(d, 3) FROM multi_rg ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, round(d, 3) FROM null_groups ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, round(d, 3) FROM typed_nulls ORDER BY id LIMIT 15": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT max(abs(d)), min(abs(d)), sum(abs(d)) FROM multi_rg": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT max(abs(d)), min(abs(d)), sum(abs(d)) FROM null_groups": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT max(abs(d)), min(abs(d)), sum(abs(d)) FROM typed_nulls": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT sum(abs(d)) FROM multi_rg": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT sum(abs(d)) FROM typed_nulls": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT sum(abs(d)) FROM typed_nulls WHERE id < 20": (
        "#1016",
        "DECIMAL expression evaluated as DOUBLE",
    ),
    "SELECT id, round(blk) FROM multi_rg ORDER BY id LIMIT 12": (
        "#1017",
        "round() of an integer returns DOUBLE",
    ),
    "SELECT id, round(blk, 1) FROM multi_rg ORDER BY id LIMIT 12": (
        "#1017",
        "round() of an integer returns DOUBLE",
    ),
    "SELECT id, round(blk, 2) FROM multi_rg ORDER BY id LIMIT 12": (
        "#1017",
        "round() of an integer returns DOUBLE",
    ),
    "SELECT id, round(id) FROM typed_nulls ORDER BY id LIMIT 12": (
        "#1017",
        "round() of an integer returns DOUBLE",
    ),
    "SELECT id, round(id, 1) FROM typed_nulls ORDER BY id LIMIT 12": (
        "#1017",
        "round() of an integer returns DOUBLE",
    ),
    "SELECT id, round(id, 2) FROM typed_nulls ORDER BY id LIMIT 12": (
        "#1017",
        "round() of an integer returns DOUBLE",
    ),
    "SELECT id, round(k) FROM multi_rg ORDER BY id LIMIT 12": (
        "#1017",
        "round() of an integer returns DOUBLE",
    ),
    "SELECT id, round(k) FROM null_groups ORDER BY id LIMIT 12": (
        "#1017",
        "round() of an integer returns DOUBLE",
    ),
    "SELECT id, round(k, 1) FROM multi_rg ORDER BY id LIMIT 12": (
        "#1017",
        "round() of an integer returns DOUBLE",
    ),
    "SELECT id, round(k, 1) FROM null_groups ORDER BY id LIMIT 12": (
        "#1017",
        "round() of an integer returns DOUBLE",
    ),
    "SELECT id, round(k, 2) FROM multi_rg ORDER BY id LIMIT 12": (
        "#1017",
        "round() of an integer returns DOUBLE",
    ),
    "SELECT id, round(k, 2) FROM null_groups ORDER BY id LIMIT 12": (
        "#1017",
        "round() of an integer returns DOUBLE",
    ),
    "SELECT id, round(n) FROM typed_nulls ORDER BY id LIMIT 12": (
        "#1017",
        "round() of an integer returns DOUBLE",
    ),
    "SELECT id, round(n, 1) FROM typed_nulls ORDER BY id LIMIT 12": (
        "#1017",
        "round() of an integer returns DOUBLE",
    ),
    "SELECT id, round(n, 2) FROM typed_nulls ORDER BY id LIMIT 12": (
        "#1017",
        "round() of an integer returns DOUBLE",
    ),
    "SELECT id, round(v) FROM null_groups ORDER BY id LIMIT 12": (
        "#1017",
        "round() of an integer returns DOUBLE",
    ),
    "SELECT id, round(v, 1) FROM null_groups ORDER BY id LIMIT 12": (
        "#1017",
        "round() of an integer returns DOUBLE",
    ),
    "SELECT id, round(v, 2) FROM null_groups ORDER BY id LIMIT 12": (
        "#1017",
        "round() of an integer returns DOUBLE",
    ),
    "SELECT sum(i64) FROM int_bounds WHERE id <= 6": (
        "#1018",
        "sum(BIGINT) overflows instead of widening",
    ),
    "SELECT sum(i64) FROM int_bounds WHERE id >= 7": (
        "#1018",
        "sum(BIGINT) overflows instead of widening",
    ),
    "SELECT sum(i64) FROM int_bounds WHERE id IN (6, 7)": (
        "#1018",
        "sum(BIGINT) overflows instead of widening",
    ),
    "SELECT b, count(*) AS n FROM typed_nulls GROUP BY b HAVING count(n) > 3": (
        "#1019",
        "HAVING count(n) resolves n to the select alias",
    ),
    "SELECT grp, b, count(*) AS n FROM typed_nulls GROUP BY grp, b HAVING count(n) > 3": (
        "#1019",
        "HAVING count(n) resolves n to the select alias",
    ),
    "SELECT grp, count(*) AS n FROM typed_nulls GROUP BY grp HAVING count(n) > 3": (
        "#1019",
        "HAVING count(n) resolves n to the select alias",
    ),
    "SELECT n % 5, count(*) AS n FROM typed_nulls GROUP BY n % 5 HAVING count(n) > 3": (
        "#1019",
        "HAVING count(n) resolves n to the select alias",
    ),
}

files: dict[str, list[str]] = {}
header: dict[str, str] = {}


def add(name: str, sql: str, sort: str | None = None) -> None:
    recs = files.setdefault(name, [])
    if sort is None:
        sort = "nosort" if "ORDER BY" in sql else "rowsort"
    rec = f"query T {sort}\n{sql}\n----\n"
    if sql in WRONG:
        issue, note = WRONG[sql]
        rec = f"# {issue}: {note}\nskipif db-core\n" + rec
    if rec not in recs:
        recs.append(rec)


def cross(*lists):
    out = [[]]
    for lst in lists:
        out = [o + [x] for o in out for x in lst]
    return out


# ---------------------------------------------------------------- aggregates
# aggregate x column x filter, per fixture. Every aggregate the engine has,
# over every column type it has it for.
AGGS_NUM = [
    "count({c})",
    "sum({c})",
    "min({c})",
    "max({c})",
    "avg({c})",
    "count(DISTINCT {c})",
]
AGG_FIXTURES = {
    "typed_nulls": (
        ["n", "f", "d"],
        [
            "",
            " WHERE n > 0",
            " WHERE grp = 'big'",
            " WHERE s IS NULL",
            " WHERE b",
            " WHERE id <= 100",
        ],
    ),
    "null_groups": (
        ["v", "x", "d", "k"],
        [
            "",
            " WHERE g = 'mixed'",
            " WHERE g = 'all_null'",
            " WHERE g IS NULL",
            " WHERE v > 0",
            " WHERE id < 0",
        ],
    ),
    "multi_rg": (
        ["k", "x", "d"],
        [
            "",
            " WHERE blk = 7",
            " WHERE blk BETWEEN 3 AND 9",
            " WHERE g = 'g1'",
            " WHERE k IS NULL",
            " WHERE id >= 29990",
        ],
    ),
}
for tbl, (cols, filters) in AGG_FIXTURES.items():
    for c in cols:
        for a in AGGS_NUM:
            for f in filters:
                add("matrix_agg_1014.test", f"SELECT {a.format(c=c)} FROM {tbl}{f}")
    for c, f in cross(cols, filters):
        add(
            "matrix_agg_1014.test",
            f"SELECT count(*), count({c}), min({c}), max({c}) FROM {tbl}{f}",
        )
for tbl, cols in [
    ("typed_nulls", ["s", "grp"]),
    ("null_groups", ["s", "g"]),
    ("multi_rg", ["s", "g"]),
]:
    for c in cols:
        for a in ["count({c})", "min({c})", "max({c})", "count(DISTINCT {c})"]:
            add("matrix_agg_1014.test", f"SELECT {a.format(c=c)} FROM {tbl}")
            add(
                "matrix_agg_1014.test",
                f"SELECT {a.format(c=c)} FROM {tbl} WHERE id < 50",
            )
for tbl in ["typed_nulls", "null_groups"]:
    for a in ["count(b)", "min(b)", "max(b)", "count(DISTINCT b)", "sum(b)"]:
        add("matrix_agg_1014.test", f"SELECT {a} FROM {tbl}")
for tbl in ["typed_nulls", "null_groups", "multi_rg", "ts_edges"]:
    for a in ["count(ts)", "count(*)", "count(DISTINCT ts)"]:
        add("matrix_agg_1014.test", f"SELECT {a} FROM {tbl}")
        add("matrix_agg_1014.test", f"SELECT {a} FROM {tbl} WHERE id < 10")

# ------------------------------------------------------------------ GROUP BY
GROUP_FIXTURES = {
    "null_groups": (["g", "k", "g, k", "k % 2", "b"], ["v", "x", "d"]),
    "typed_nulls": (["grp", "b", "grp, b", "n % 5"], ["n", "f", "d"]),
    "multi_rg": (["g", "blk", "g, blk % 3", "k % 4", "blk // 5"], ["k", "x", "d"]),
}
for tbl, (keys, cols) in GROUP_FIXTURES.items():
    for key in keys:
        for c in cols:
            agg = f"count(*), count({c}), sum({c}), min({c}), max({c}), avg({c})"
            add(
                "matrix_group_by_1014.test",
                f"SELECT {key}, {agg} FROM {tbl} GROUP BY {key}",
            )
            add(
                "matrix_group_by_1014.test",
                f"SELECT {key}, {agg} FROM {tbl} GROUP BY {key} ORDER BY {key}",
            )
            add(
                "matrix_group_by_1014.test",
                f"SELECT {key}, sum({c}) FROM {tbl} WHERE {c} IS NOT NULL GROUP BY {key}",
            )
            add(
                "matrix_group_by_1014.test",
                f"SELECT {key}, count(*) AS n FROM {tbl} GROUP BY {key} HAVING count({c}) > 3",
            )
            add(
                "matrix_group_by_1014.test",
                f"SELECT {key}, max({c}) FROM {tbl} GROUP BY {key} HAVING sum({c}) IS NOT NULL",
            )
            add(
                "matrix_group_by_1014.test",
                f"SELECT {key}, count(DISTINCT {c}) FROM {tbl} GROUP BY {key}",
            )
            add(
                "matrix_group_by_1014.test",
                f"SELECT {key}, count(*) FROM {tbl} GROUP BY {key} ORDER BY count(*) DESC, {key} LIMIT 3",
            )
            add(
                "matrix_group_by_1014.test",
                f"SELECT {key}, sum({c}) FROM {tbl} GROUP BY {key} ORDER BY sum({c}), {key} LIMIT 4",
            )
for key in ["region", "id % 4", "id % 7"]:
    for agg in [
        "count(*)",
        "sum(amount)",
        "min(amount)",
        "max(amount)",
        "avg(amount)",
        "count(DISTINCT id)",
    ]:
        add(
            "matrix_group_by_1014.test",
            f"SELECT {key}, {agg} FROM production GROUP BY {key}",
        )
        add(
            "matrix_group_by_1014.test",
            f"SELECT {key}, {agg} FROM production WHERE id > 2500 GROUP BY {key} ORDER BY {key}",
        )
        add(
            "matrix_group_by_1014.test",
            f"SELECT {key}, {agg} FROM production GROUP BY {key} HAVING count(*) > 700 ORDER BY {key}",
        )
for tbl, key in [("null_groups", "g"), ("typed_nulls", "grp"), ("multi_rg", "g")]:
    for expr in [
        "sum(CASE WHEN {v} > 0 THEN 1 ELSE 0 END)",
        "count(CASE WHEN {v} > 0 THEN 1 END)",
        "sum(CASE WHEN {v} IS NULL THEN 1 ELSE 0 END)",
        "max(coalesce({v}, 0))",
        "min(coalesce({v}, 0))",
        "sum({v} + 1)",
        "sum({v}) + 1",
        "max({v}) - min({v})",
        "sum(abs({v}))",
        "count(*) FILTER (WHERE {v} > 0)",
    ]:
        v = {"null_groups": "v", "typed_nulls": "n", "multi_rg": "k"}[tbl]
        add(
            "matrix_group_by_1014.test",
            f"SELECT {key}, {expr.format(v=v)} FROM {tbl} GROUP BY {key}",
        )


# ------------------------------------------------------------------- filters
def preds(t: str) -> list[str]:
    if t == "null_groups":
        cols_n = ["v", "k", "id"]
        cols_f = ["x", "d"]
        out = []
        for c in cols_n:
            for op in ["=", "<>", "<", "<=", ">", ">="]:
                out.append(f"{c} {op} 5")
            out += [
                f"{c} IN (0, 1, 2, 3)",
                f"{c} NOT IN (0, 1, 2, 3)",
                f"{c} BETWEEN -5 AND 5",
                f"{c} NOT BETWEEN -5 AND 5",
                f"{c} IS NULL",
                f"{c} IS NOT NULL",
            ]
        for c in cols_f:
            for op in ["=", "<>", "<", "<=", ">", ">="]:
                out.append(f"{c} {op} 1.5")
            out += [f"{c} BETWEEN 0 AND 5", f"{c} IS NULL", f"{c} IS NOT NULL"]
        out += [
            "b",
            "NOT b",
            "b = true",
            "b = false",
            "b IS NULL",
            "b IS NOT NULL",
            "g = 'mixed'",
            "g <> 'mixed'",
            "g IS NULL",
            "g IN ('neg', 'zeros')",
            "g NOT IN ('neg', 'zeros')",
            "s = 'solo'",
            "s = ''",
            "s <> ''",
            "s IS NULL",
            "s LIKE 'm1%'",
            "s NOT LIKE 'm1%'",
            "s LIKE '%1'",
            "s LIKE 'n_'",
            "s ILIKE 'NEG%'",
            "s > 'm5'",
            "s < 'n'",
            "v > 0 AND x > 0",
            "v > 0 OR x > 0",
            "NOT (v > 0 AND x > 0)",
            "v > 0 AND NOT b",
            "(v > 0 OR b) AND g = 'mixed'",
            "v > k",
            "v + k > 10",
            "v * 2 < k",
            "abs(v) > 10",
            "round(x) = 3",
            "v = x",
            "d < v",
            "v > 1.5",
            "g = 'mixed' AND b IS NULL",
            "g IS NULL AND v IS NOT NULL",
            "v IS NULL AND x IS NULL",
            "v IS NULL OR x IS NULL",
            "coalesce(v, 0) = 0",
            "nullif(v, 0) IS NULL",
            "ts > '2026-03-02'",
            "ts <= '2026-03-02 12:00:00'",
            "ts IS NULL",
            "ts BETWEEN '2026-03-02' AND '2026-03-04'",
        ]
        return out
    if t == "typed_nulls":
        out = []
        for c, lit in [("n", "0"), ("f", "100.5"), ("d", "0.00"), ("id", "5000")]:
            for op in ["=", "<>", "<", "<=", ">", ">="]:
                out.append(f"{c} {op} {lit}")
            out += [f"{c} IS NULL", f"{c} IS NOT NULL"]
        out += [
            "n IN (1, 2, 3)",
            "n NOT IN (1, 2, 3)",
            "n BETWEEN -50 AND 50",
            "f BETWEEN 10 AND 20",
            "d BETWEEN -1 AND 1",
            "b",
            "NOT b",
            "b IS NULL",
            "grp = 'big'",
            "grp <> 'big'",
            "grp IS NULL",
            "grp IN ('small', 'tiny')",
            "s IS NULL",
            "s = 'delta3'",
            "s LIKE 'alpha%'",
            "s NOT LIKE 'alpha%'",
            "s ILIKE 'GAMMA%'",
            "s LIKE '%9'",
            "n > 0 AND f < 100",
            "n > 0 OR f < 100",
            "NOT (n > 0)",
            "n > 0 AND b",
            "grp = 'big' AND n IS NULL",
            "n IS NULL AND f IS NULL",
            "ts >= '2026-06-01'",
            "ts < '2026-01-10 12:00:00'",
            "ts IS NULL",
            "ts BETWEEN '2026-02-01' AND '2026-02-15'",
            "n + 1 > 10",
            "n % 3 = 0",
            "n // 7 = 3",
            "abs(n) < 5",
            "f * 2 > 500",
            "n > f",
            "coalesce(n, 0) > 0",
        ]
        return out
    if t == "multi_rg":
        out = []
        for c, lit in [
            ("id", "15000"),
            ("blk", "12"),
            ("k", "0"),
            ("x", "100.0"),
            ("d", "500.00"),
        ]:
            for op in ["=", "<>", "<", "<=", ">", ">="]:
                out.append(f"{c} {op} {lit}")
            out += [f"{c} IS NULL", f"{c} IS NOT NULL"]
        out += [
            "blk IN (1, 5, 29)",
            "blk BETWEEN 10 AND 12",
            "blk = 0 AND k > 0",
            "blk >= 28 OR blk <= 1",
            "id BETWEEN 1990 AND 2060",
            "id = 2047",
            "id = 2048",
            "id = 4095",
            "id = 4096",
            "id >= 29999",
            "id < 0",
            "id > 29999",
            "g = 'g0'",
            "g IN ('g1', 'g2')",
            "g IS NULL",
            "g <> 'g0'",
            "s = 's5'",
            "s LIKE 's1%'",
            "s IS NULL",
            "k > 0 AND x > 100",
            "k > 0 OR x > 100",
            "blk = 3 AND g = 'g3'",
            "blk % 2 = 0",
            "k % 7 = 0",
            "x IS NULL AND d IS NULL",
            "ts >= '2026-01-05'",
            "ts < '2026-01-02'",
            "ts BETWEEN '2026-01-10' AND '2026-01-11'",
        ]
        return out
    return []


for t in ["null_groups", "typed_nulls", "multi_rg"]:
    for p in preds(t):
        add("matrix_filter_1014.test", f"SELECT count(*) FROM {t} WHERE {p}")
        add(
            "matrix_filter_1014.test",
            f"SELECT id FROM {t} WHERE {p} ORDER BY id LIMIT 12",
        )
for p in [
    "region = 'east'",
    "region <> 'east'",
    "region IN ('east', 'west')",
    "region NOT IN ('east', 'west')",
    "amount > 6000",
    "amount <= 100",
    "amount BETWEEN 100 AND 200",
    "id > 4990",
    "id < 10",
    "id % 100 = 0",
    "region = 'east' AND amount > 6000",
    "region = 'east' OR amount > 12000",
    "NOT (region = 'east')",
    "region LIKE 'e%'",
    "region NOT LIKE '%st'",
    "amount * 2 > 20000",
    "amount + id > 17000",
    "abs(amount - 6000) < 10",
]:
    add("matrix_filter_1014.test", f"SELECT count(*) FROM production WHERE {p}")
    add(
        "matrix_filter_1014.test",
        f"SELECT id FROM production WHERE {p} ORDER BY id LIMIT 12",
    )

# --------------------------------------------------------- ORDER BY / LIMIT
# `SELECT *` minus the TIMESTAMP column (printed as microseconds, see above).
STAR = {
    "null_groups": "id, g, k, v, x, d, s, b",
    "typed_nulls": "id, n, f, s, b, d, grp",
    "multi_rg": "id, blk, g, k, x, d, s",
    "production": "*",
}
ORDER_FIXTURES = {
    "null_groups": ["v", "x", "d", "g", "s", "k", "b"],
    "typed_nulls": ["n", "f", "d", "s", "grp", "b"],
    "multi_rg": ["k", "x", "d", "g", "s", "blk"],
    "production": ["region", "amount"],
}
for tbl, cols in ORDER_FIXTURES.items():
    for c in cols:
        for d in ["", " DESC"]:
            for lim in [
                "LIMIT 5",
                "LIMIT 0",
                "LIMIT 6 OFFSET 7",
                "LIMIT 10 OFFSET 1000000",
            ]:
                add(
                    "matrix_order_1014.test",
                    f"SELECT id, {c} FROM {tbl} ORDER BY {c}{d}, id {lim}",
                )
        for c2 in cols[:4]:
            if c2 != c:
                add(
                    "matrix_order_1014.test",
                    f"SELECT id FROM {tbl} ORDER BY {c}, {c2} DESC, id LIMIT 5",
                )
                add(
                    "matrix_order_1014.test",
                    f"SELECT id FROM {tbl} ORDER BY {c} DESC, {c2}, id LIMIT 5",
                )
        add(
            "matrix_order_1014.test",
            f"SELECT DISTINCT {c} FROM {tbl} ORDER BY {c} LIMIT 20",
        )
        add(
            "matrix_order_1014.test",
            f"SELECT DISTINCT {c} FROM {tbl} ORDER BY {c} DESC LIMIT 6",
        )
        add("matrix_order_1014.test", f"SELECT {c} FROM {tbl} ORDER BY 1, id LIMIT 8")
    add("matrix_order_1014.test", f"SELECT id FROM {tbl} ORDER BY id DESC LIMIT 9")
    add("matrix_order_1014.test", f"SELECT {STAR[tbl]} FROM {tbl} WHERE id = 3")
for tbl, c in [("null_groups", "v"), ("typed_nulls", "n"), ("multi_rg", "k")]:
    add(
        "matrix_order_1014.test",
        f"SELECT id, {c} + 1 AS e FROM {tbl} ORDER BY e, id LIMIT 10",
    )
    add(
        "matrix_order_1014.test",
        f"SELECT id, abs({c}) AS a FROM {tbl} ORDER BY a DESC, id LIMIT 10",
    )
    add(
        "matrix_order_1014.test",
        f"SELECT id, coalesce({c}, 0) AS z FROM {tbl} ORDER BY z, id LIMIT 10",
    )

# --------------------------------------------------- arithmetic / abs / round
ARITH = {
    "null_groups": (["v", "k"], ["x"], ["d"]),
    "typed_nulls": (["n", "id"], ["f"], ["d"]),
    "multi_rg": (["k", "blk"], ["x"], ["d"]),
}
for tbl, (ints, flts, decs) in ARITH.items():
    cols = ints + flts + decs
    for a in cols:
        for lit in ["1", "2", "7", "-3", "0"]:
            for op in ["+", "-", "*", "/", "%"]:
                add(
                    "matrix_arith_1014.test",
                    f"SELECT id, {a} {op} {lit} FROM {tbl} ORDER BY id LIMIT 12",
                )
        add(
            "matrix_arith_1014.test",
            f"SELECT id, -{a}, abs({a}) FROM {tbl} ORDER BY id LIMIT 12",
        )
        add(
            "matrix_arith_1014.test",
            f"SELECT id, abs({a}) + abs(-1) FROM {tbl} ORDER BY id LIMIT 12",
        )
        add(
            "matrix_arith_1014.test",
            f"SELECT max(abs({a})), min(abs({a})), sum(abs({a})) FROM {tbl}",
        )
    for i in range(len(cols)):
        for j in range(len(cols)):
            if i != j:
                for op in ["+", "-", "*"]:
                    add(
                        "matrix_arith_1014.test",
                        f"SELECT id, {cols[i]} {op} {cols[j]} FROM {tbl} ORDER BY id LIMIT 10",
                    )
    for a in ints + flts:
        for n in ["", ", 1", ", 2"]:
            add(
                "matrix_arith_1014.test",
                f"SELECT id, round({a}{n}) FROM {tbl} ORDER BY id LIMIT 12",
            )
    for a in cols:
        add(
            "matrix_arith_1014.test",
            f"SELECT sum({a} * 2), sum({a}) * 2, avg({a} + 1), max({a} - 1) FROM {tbl}",
        )
        add(
            "matrix_arith_1014.test",
            f"SELECT count(*) FROM {tbl} WHERE {a} * 2 > {a} + 1",
        )

# ------------------------------------------------------------------- NULLs
for tbl, (n1, n2, n3) in {
    "null_groups": ("v", "x", "d"),
    "typed_nulls": ("n", "f", "d"),
    "multi_rg": ("k", "x", "d"),
}.items():
    ops = ["=", "<>", "<", ">", "<=", ">="]
    for op in ops:
        add(
            "matrix_null_1014.test",
            f"SELECT id, {n1} {op} {n2} FROM {tbl} ORDER BY id LIMIT 20",
        )
        add("matrix_null_1014.test", f"SELECT count(*) FROM {tbl} WHERE {n1} {op} {n2}")
        add(
            "matrix_null_1014.test",
            f"SELECT count(*) FROM {tbl} WHERE NOT ({n1} {op} {n2})",
        )
        add("matrix_null_1014.test", f"SELECT count(*) FROM {tbl} WHERE {n1} {op} NULL")
    for a, b in [(n1, n2), (n1, n3), (n2, n3)]:
        for conn in ["AND", "OR"]:
            add(
                "matrix_null_1014.test",
                f"SELECT id, {a} > 0 {conn} {b} > 0 FROM {tbl} ORDER BY id LIMIT 20",
            )
            add(
                "matrix_null_1014.test",
                f"SELECT id, {a} IS NULL {conn} {b} IS NULL FROM {tbl} ORDER BY id LIMIT 20",
            )
            add(
                "matrix_null_1014.test",
                f"SELECT count(*) FROM {tbl} WHERE {a} > 0 {conn} {b} > 0",
            )
            add(
                "matrix_null_1014.test",
                f"SELECT count(*) FROM {tbl} WHERE NOT ({a} > 0 {conn} {b} > 0)",
            )
        add(
            "matrix_null_1014.test",
            f"SELECT id, NOT ({a} > 0) FROM {tbl} ORDER BY id LIMIT 20",
        )
        add(
            "matrix_null_1014.test",
            f"SELECT id, {a} IS NULL, {b} IS NOT NULL FROM {tbl} ORDER BY id LIMIT 20",
        )
        add(
            "matrix_null_1014.test",
            f"SELECT count(*) FROM {tbl} WHERE {a} IS NULL AND {b} IS NOT NULL",
        )
        add(
            "matrix_null_1014.test",
            f"SELECT id, {a} + {b} FROM {tbl} ORDER BY id LIMIT 20",
        )
    for c in [n1, n2, n3]:
        for lit in ["0", "-1", "99"]:
            add(
                "matrix_null_1014.test",
                f"SELECT id, coalesce({c}, {lit}) FROM {tbl} ORDER BY id LIMIT 20",
            )
            add(
                "matrix_null_1014.test",
                f"SELECT id, ifnull({c}, {lit}) FROM {tbl} ORDER BY id LIMIT 20",
            )
            add(
                "matrix_null_1014.test",
                f"SELECT id, nullif({c}, {lit}) FROM {tbl} ORDER BY id LIMIT 20",
            )
            add(
                "matrix_null_1014.test",
                f"SELECT count(*), count(nullif({c}, {lit})), sum(coalesce({c}, {lit})) FROM {tbl}",
            )
        add(
            "matrix_null_1014.test",
            f"SELECT id, CASE WHEN {c} > 0 THEN 'pos' WHEN {c} < 0 THEN 'neg' ELSE 'zero' END FROM {tbl} ORDER BY id LIMIT 20",
        )
        add(
            "matrix_null_1014.test",
            f"SELECT id, CASE WHEN {c} IS NULL THEN 'null' ELSE 'val' END FROM {tbl} ORDER BY id LIMIT 20",
        )
        add(
            "matrix_null_1014.test",
            f"SELECT id, CASE {c} WHEN 0 THEN 'zero' ELSE 'other' END FROM {tbl} ORDER BY id LIMIT 20",
        )
        add(
            "matrix_null_1014.test",
            f"SELECT id, CASE WHEN {c} > 0 THEN {c} END FROM {tbl} ORDER BY id LIMIT 20",
        )
        add(
            "matrix_null_1014.test",
            f"SELECT count(*), count({c}), count(*) - count({c}) FROM {tbl}",
        )
        add(
            "matrix_null_1014.test",
            f"SELECT coalesce({c}, 0) = 0, count(*) FROM {tbl} GROUP BY coalesce({c}, 0) = 0",
        )
        add(
            "matrix_null_1014.test",
            f"SELECT {c} IS NULL, count(*), sum({c}) FROM {tbl} GROUP BY {c} IS NULL",
        )
    add(
        "matrix_null_1014.test",
        f"SELECT coalesce({n1}, {n2}, {n3}, -1) FROM {tbl} ORDER BY id LIMIT 30",
    )
    add(
        "matrix_null_1014.test",
        f"SELECT count(*) FROM {tbl} WHERE {n1} IS NULL AND {n2} IS NULL AND {n3} IS NULL",
    )
    add(
        "matrix_null_1014.test",
        f"SELECT count(*) FROM {tbl} WHERE {n1} IS NULL OR {n2} IS NULL OR {n3} IS NULL",
    )
    add("matrix_null_1014.test", f"SELECT count(*) FROM {tbl} WHERE NULL")
    add("matrix_null_1014.test", f"SELECT count(*) FROM {tbl} WHERE NOT NULL")
    add("matrix_null_1014.test", f"SELECT count(*) FROM {tbl} WHERE NULL IS NULL")
    add("matrix_null_1014.test", f"SELECT count(*) FROM {tbl} WHERE {n1} = {n1}")
    add("matrix_null_1014.test", f"SELECT count(*) FROM {tbl} WHERE {n1} IN (1, NULL)")
    add(
        "matrix_null_1014.test",
        f"SELECT count(*) FROM {tbl} WHERE {n1} NOT IN (1, NULL)",
    )
    add("matrix_null_1014.test", f"SELECT count(*) FROM {tbl} WHERE {n1} IN (1, 2, 3)")
    add(
        "matrix_null_1014.test",
        f"SELECT count(*) FROM {tbl} WHERE {n1} NOT IN (1, 2, 3)",
    )
for tbl, c in [("null_groups", "g"), ("typed_nulls", "grp"), ("multi_rg", "g")]:
    for expr in [f"{c}", f"coalesce({c}, 'none')", f"{c} || 'x'", f"{c} IS NULL"]:
        add(
            "matrix_null_1014.test",
            f"SELECT {expr}, count(*) FROM {tbl} GROUP BY {expr}",
        )
    add(
        "matrix_null_1014.test",
        f"SELECT count(*), count({c}), count(DISTINCT {c}) FROM {tbl}",
    )
    add(
        "matrix_null_1014.test",
        f"SELECT id, {c} = {c}, {c} <> 'x' FROM {tbl} ORDER BY id LIMIT 20",
    )
    add(
        "matrix_null_1014.test",
        f"SELECT id, coalesce({c}, 'none') FROM {tbl} ORDER BY id LIMIT 20",
    )
    add(
        "matrix_null_1014.test",
        f"SELECT id, nullif({c}, 'g1') FROM {tbl} ORDER BY id LIMIT 20",
    )

# ----------------------------------------------------------------- DECIMAL
for tbl, c in [("null_groups", "d"), ("typed_nulls", "d"), ("multi_rg", "d")]:
    for lit in ["0", "1", "2", "3", "7", "0.5", "1.25", "-1", "-0.01", "100"]:
        for op in ["+", "-", "*", "/"]:
            add(
                "matrix_decimal_1014.test",
                f"SELECT id, {c} {op} {lit} FROM {tbl} ORDER BY id LIMIT 15",
            )
    for op in ["=", "<>", "<", "<=", ">", ">="]:
        for lit in ["0", "0.5", "1.00", "-1.5", "100"]:
            add(
                "matrix_decimal_1014.test",
                f"SELECT count(*) FROM {tbl} WHERE {c} {op} {lit}",
            )
    for fn in [
        "sum({c})",
        "avg({c})",
        "min({c})",
        "max({c})",
        "count({c})",
        "count(DISTINCT {c})",
        "sum({c} * {c})",
        "sum({c} + 1)",
        "avg({c} * 2)",
        "sum({c}) / 3",
        "max({c}) - min({c})",
        "sum(abs({c}))",
        "sum(coalesce({c}, 0))",
    ]:
        add("matrix_decimal_1014.test", f"SELECT {fn.format(c=c)} FROM {tbl}")
        add(
            "matrix_decimal_1014.test",
            f"SELECT {fn.format(c=c)} FROM {tbl} WHERE id < 20",
        )
    for n in ["0", "1", "2", "3"]:
        add(
            "matrix_decimal_1014.test",
            f"SELECT id, round({c}, {n}) FROM {tbl} ORDER BY id LIMIT 15",
        )
    add(
        "matrix_decimal_1014.test",
        f"SELECT id, round({c}) FROM {tbl} ORDER BY id LIMIT 15",
    )
    add(
        "matrix_decimal_1014.test",
        f"SELECT id, -{c}, abs({c}), {c} * -1 FROM {tbl} ORDER BY id LIMIT 15",
    )
for tbl, a, b in [
    ("null_groups", "d", "x"),
    ("null_groups", "d", "v"),
    ("typed_nulls", "d", "n"),
    ("typed_nulls", "d", "f"),
    ("multi_rg", "d", "x"),
    ("multi_rg", "d", "k"),
]:
    for op in ["+", "-", "*", "/", "<", ">", "="]:
        add(
            "matrix_decimal_1014.test",
            f"SELECT id, {a} {op} {b} FROM {tbl} ORDER BY id LIMIT 15",
        )
        add(
            "matrix_decimal_1014.test", f"SELECT count(*) FROM {tbl} WHERE {a} {op} {b}"
        )

# --------------------------------------------------------------- BOUNDARIES
for c in ["i8", "i16", "i32", "i64", "f", "d"]:
    for a in [
        "count({c})",
        "min({c})",
        "max({c})",
        "sum({c})",
        "avg({c})",
        "count(DISTINCT {c})",
    ]:
        add("matrix_bounds_1014.test", f"SELECT {a.format(c=c)} FROM int_bounds")
        add(
            "matrix_bounds_1014.test",
            f"SELECT {a.format(c=c)} FROM int_bounds WHERE id <= 6",
        )
        add(
            "matrix_bounds_1014.test",
            f"SELECT {a.format(c=c)} FROM int_bounds WHERE id >= 7",
        )
    for op in ["=", "<>", "<", "<=", ">", ">="]:
        for lit in ["0", "1", "-1"]:
            add(
                "matrix_bounds_1014.test",
                f"SELECT id FROM int_bounds WHERE {c} {op} {lit} ORDER BY id",
            )
    add("matrix_bounds_1014.test", f"SELECT id, {c} FROM int_bounds ORDER BY {c}, id")
    add(
        "matrix_bounds_1014.test",
        f"SELECT id, {c} FROM int_bounds ORDER BY {c} DESC, id",
    )
    add("matrix_bounds_1014.test", f"SELECT id, {c} FROM int_bounds ORDER BY id")
    add(
        "matrix_bounds_1014.test",
        f"SELECT id, -{c} FROM int_bounds WHERE id NOT IN (1, 2) ORDER BY id",
    )
    add(
        "matrix_bounds_1014.test",
        f"SELECT id, abs({c}) FROM int_bounds WHERE id NOT IN (1) ORDER BY id",
    )
    add("matrix_bounds_1014.test", f"SELECT {c} FROM int_bounds WHERE {c} IS NULL")
    add("matrix_bounds_1014.test", f"SELECT {c}, count(*) FROM int_bounds GROUP BY {c}")
for lo, hi in [
    ("-128", "127"),
    ("-32768", "32767"),
    ("-2147483648", "2147483647"),
    ("-9223372036854775808", "9223372036854775807"),
]:
    for c in ["i8", "i16", "i32", "i64"]:
        add(
            "matrix_bounds_1014.test",
            f"SELECT id FROM int_bounds WHERE {c} = {lo} OR {c} = {hi} ORDER BY id",
        )
        add(
            "matrix_bounds_1014.test",
            f"SELECT id FROM int_bounds WHERE {c} BETWEEN {lo} AND {hi} ORDER BY id",
        )
        add(
            "matrix_bounds_1014.test",
            f"SELECT id FROM int_bounds WHERE {c} > {lo} AND {c} < {hi} ORDER BY id",
        )
for a, b in [("i8", "i16"), ("i16", "i32"), ("i32", "i64"), ("i8", "i64")]:
    for op in ["=", "<", ">"]:
        add(
            "matrix_bounds_1014.test",
            f"SELECT id FROM int_bounds WHERE {a} {op} {b} ORDER BY id",
        )
    add(
        "matrix_bounds_1014.test",
        f"SELECT id, {a} + {b} FROM int_bounds WHERE id IN (3, 4, 5, 9) ORDER BY id",
    )
    add(
        "matrix_bounds_1014.test",
        f"SELECT id, {a} * {b} FROM int_bounds WHERE id IN (3, 4, 5, 9) ORDER BY id",
    )
add(
    "matrix_bounds_1014.test",
    "SELECT sum(i64), sum(i32), sum(i16), sum(i8) FROM int_bounds",
)
add("matrix_bounds_1014.test", "SELECT sum(i64) FROM int_bounds WHERE id IN (6, 7)")
add("matrix_bounds_1014.test", "SELECT sum(i32) FROM int_bounds WHERE id IN (6, 7)")
add("matrix_bounds_1014.test", "SELECT sum(i64) FROM int_bounds WHERE id IN (1, 7)")
add("matrix_bounds_1014.test", "SELECT avg(i64) FROM int_bounds WHERE id IN (6, 7)")
add(
    "matrix_bounds_1014.test",
    "SELECT id, i64 // 2, i64 % 1000 FROM int_bounds ORDER BY id",
)
add("matrix_bounds_1014.test", "SELECT id, i64 / 2 FROM int_bounds ORDER BY id")
add(
    "matrix_bounds_1014.test",
    "SELECT id, i32 // 3, i32 % 3 FROM int_bounds ORDER BY id",
)
add(
    "matrix_bounds_1014.test",
    "SELECT id, f * 2, f / 2, f + 1 FROM int_bounds WHERE id IN (3, 4, 5, 6, 9, 11, 12) ORDER BY id",
)
add("matrix_bounds_1014.test", "SELECT id FROM int_bounds WHERE f = 0.0 ORDER BY id")
add("matrix_bounds_1014.test", "SELECT id FROM int_bounds WHERE f = -0.0 ORDER BY id")
add(
    "matrix_bounds_1014.test",
    "SELECT id FROM int_bounds WHERE f + 0.2 = 0.30000000000000004 ORDER BY id",
)
add("matrix_bounds_1014.test", "SELECT count(DISTINCT f) FROM int_bounds")
add("matrix_bounds_1014.test", "SELECT f, count(*) FROM int_bounds GROUP BY f")
add("matrix_bounds_1014.test", "SELECT id FROM int_bounds ORDER BY f, id")
add(
    "matrix_bounds_1014.test",
    "SELECT id, d + 0.001, d - 0.001 FROM int_bounds WHERE id IN (2, 3, 4, 5, 6, 9, 10, 11, 12) ORDER BY id",
)
add(
    "matrix_bounds_1014.test",
    "SELECT id, d * 2 FROM int_bounds WHERE id IN (2, 3, 4, 5, 6, 9, 10, 11, 12) ORDER BY id",
)
add(
    "matrix_bounds_1014.test",
    "SELECT id, d / 3 FROM int_bounds WHERE id IN (2, 3, 4, 5, 6, 9, 10, 11, 12) ORDER BY id",
)
add("matrix_bounds_1014.test", "SELECT sum(d) FROM int_bounds WHERE id IN (1, 7)")
add("matrix_bounds_1014.test", "SELECT sum(d) FROM int_bounds WHERE id IN (7, 12)")

# --------------------------------------------------------------- TIMESTAMPS
for tbl in ["ts_edges", "typed_nulls", "null_groups", "multi_rg"]:
    for op in ["=", "<>", "<", "<=", ">", ">="]:
        for lit in [
            "1970-01-01",
            "1970-01-01 00:00:00",
            "2000-02-29 12:00:00",
            "2026-03-04",
            "2026-06-15 12:30:45.123456",
            "2038-01-19 03:14:07",
            "2100-03-01",
        ]:
            add(
                "matrix_timestamp_1014.test",
                f"SELECT count(*) FROM {tbl} WHERE ts {op} '{lit}'",
            )
    add("matrix_timestamp_1014.test", f"SELECT id FROM {tbl} ORDER BY ts, id LIMIT 12")
    add(
        "matrix_timestamp_1014.test",
        f"SELECT id FROM {tbl} ORDER BY ts DESC, id LIMIT 12",
    )
    add(
        "matrix_timestamp_1014.test",
        f"SELECT id FROM {tbl} WHERE ts IS NULL ORDER BY id LIMIT 12",
    )
    add(
        "matrix_timestamp_1014.test",
        f"SELECT count(*), count(ts), count(DISTINCT ts) FROM {tbl}",
    )
    for unit in [
        "microsecond",
        "second",
        "minute",
        "hour",
        "day",
        "week",
        "month",
        "quarter",
        "year",
    ]:
        add(
            "matrix_timestamp_1014.test",
            f"SELECT count(DISTINCT date_trunc('{unit}', ts)) FROM {tbl}",
        )
        add(
            "matrix_timestamp_1014.test",
            f"SELECT count(*) FROM {tbl} WHERE date_trunc('{unit}', ts) = date_trunc('{unit}', ts)",
        )
    for lo, hi in [
        ("2026-01-01", "2026-12-31"),
        ("1969-12-31", "1970-01-02"),
        ("1900-01-01", "2100-12-31"),
        ("2026-06-15 12:30:45", "2026-06-15 12:30:46"),
    ]:
        add(
            "matrix_timestamp_1014.test",
            f"SELECT count(*) FROM {tbl} WHERE ts BETWEEN '{lo}' AND '{hi}'",
        )
        add(
            "matrix_timestamp_1014.test",
            f"SELECT count(*) FROM {tbl} WHERE ts NOT BETWEEN '{lo}' AND '{hi}'",
        )
for hi in ["1970-01-01", "2000-01-01", "2038-01-19 03:14:08", "9999-12-31"]:
    add(
        "matrix_timestamp_1014.test",
        f"SELECT id FROM ts_edges WHERE ts < '{hi}' ORDER BY ts, id",
    )
    add(
        "matrix_timestamp_1014.test",
        f"SELECT id FROM ts_edges WHERE ts >= '{hi}' ORDER BY ts DESC, id",
    )
for unit in ["day", "month", "year", "hour"]:
    add(
        "matrix_timestamp_1014.test",
        f"SELECT count(*) FROM ts_edges WHERE date_trunc('{unit}', ts) = '1970-01-01'",
    )
    add(
        "matrix_timestamp_1014.test",
        f"SELECT count(*) FROM ts_edges WHERE date_trunc('{unit}', ts) < '2000-01-01'",
    )
    add(
        "matrix_timestamp_1014.test",
        f"SELECT count(*) FROM ts_edges WHERE date_trunc('{unit}', ts) >= '2026-01-01'",
    )
    add(
        "matrix_timestamp_1014.test",
        f"SELECT count(*) FROM multi_rg WHERE date_trunc('{unit}', ts) = '2026-01-02'",
    )
    add(
        "matrix_timestamp_1014.test",
        f"SELECT count(*) FROM multi_rg WHERE date_trunc('{unit}', ts) = '2026-01-01'",
    )
for unit in ["day", "month", "hour"]:
    for tbl, v in [("null_groups", "v"), ("typed_nulls", "n"), ("multi_rg", "k")]:
        add(
            "matrix_timestamp_1014.test",
            f"SELECT count(DISTINCT date_trunc('{unit}', ts)), sum({v}), count(*) FROM {tbl}",
        )
        add(
            "matrix_timestamp_1014.test",
            f"SELECT count(*), sum({v}) FROM {tbl} GROUP BY date_trunc('{unit}', ts) HAVING count(*) > 20 ORDER BY 1 DESC, 2 DESC LIMIT 5",
        )

# ------------------------------------------------------------------ STRINGS
STR_FIXTURES = {
    "null_groups": "s",
    "typed_nulls": "s",
    "multi_rg": "s",
    "production": "region",
}
for tbl, c in STR_FIXTURES.items():
    for pat in [
        "a%",
        "%a",
        "%a%",
        "_%",
        "m1%",
        "s1_",
        "%1",
        "n_",
        "%",
        "",
        "x%",
        "%1_",
        "__",
        "S%",
        "NEG%",
        "e%",
        "%st",
    ]:
        for fn in ["LIKE", "NOT LIKE", "ILIKE"]:
            add(
                "matrix_string_1014.test",
                f"SELECT count(*) FROM {tbl} WHERE {c} {fn} '{pat}'",
            )
    for lit in ["a", "solo", "", "s5", "east", "m1", "zzz", "alpha4", "GAMMA2"]:
        for op in ["=", "<>", "<", "<=", ">", ">="]:
            add(
                "matrix_string_1014.test",
                f"SELECT count(*) FROM {tbl} WHERE {c} {op} '{lit}'",
            )
    for expr in [
        f"{c} || 'x'",
        f"'x' || {c}",
        f"{c} || {c}",
        f"len({c})",
        f"len({c}) + 1",
        f"coalesce({c}, '?') || '!'",
    ]:
        add(
            "matrix_string_1014.test",
            f"SELECT id, {expr} FROM {tbl} ORDER BY id LIMIT 15",
        )
    add("matrix_string_1014.test", f"SELECT count(*) FROM {tbl} WHERE len({c}) > 2")
    add("matrix_string_1014.test", f"SELECT count(*) FROM {tbl} WHERE len({c}) = 0")
    add(
        "matrix_string_1014.test",
        f"SELECT max(len({c})), min(len({c})), sum(len({c})) FROM {tbl}",
    )
    add(
        "matrix_string_1014.test",
        f"SELECT len({c}), count(*) FROM {tbl} GROUP BY len({c})",
    )
    add("matrix_string_1014.test", f"SELECT id FROM {tbl} ORDER BY {c}, id LIMIT 10")
    add(
        "matrix_string_1014.test",
        f"SELECT id FROM {tbl} ORDER BY {c} DESC, id LIMIT 10",
    )
    add("matrix_string_1014.test", f"SELECT {c}, count(*) FROM {tbl} GROUP BY {c}")
    add(
        "matrix_string_1014.test",
        f"SELECT {c}, count(*) FROM {tbl} GROUP BY {c} HAVING count(*) > 1",
    )
    add(
        "matrix_string_1014.test",
        f"SELECT min({c}), max({c}), count(DISTINCT {c}) FROM {tbl}",
    )
    add(
        "matrix_string_1014.test", f"SELECT min({c}), max({c}) FROM {tbl} WHERE id < 10"
    )
    add(
        "matrix_string_1014.test",
        f"SELECT id FROM {tbl} WHERE {c} IN ('a', 'solo', 's5', 'east') ORDER BY id LIMIT 10",
    )
    add(
        "matrix_string_1014.test",
        f"SELECT id FROM {tbl} WHERE {c} NOT IN ('a', 'solo', 's5', 'east') ORDER BY id LIMIT 10",
    )
    add(
        "matrix_string_1014.test",
        f"SELECT id FROM {tbl} WHERE {c} BETWEEN 'a' AND 'n' ORDER BY id LIMIT 10",
    )

# ------------------------------------------------------------- unsupported
# Pinned by hand in errors_1014.test; nothing generated.


def write(out: pathlib.Path) -> None:
    for name, recs in sorted(files.items()):
        head = header.get(name, "")
        body = "\n".join(recs)
        out.joinpath(name).write_text(
            f"# Generated by tools/gen_column_sqllogictest.py (db-core#1014); expectations\n"
            f"# written by `make refresh-column-sqllogictest` from DuckDB, never by hand.\n"
            f"{head}\n{body}"
        )


if __name__ == "__main__":
    out = pathlib.Path(sys.argv[1])
    write(out)
    total = sum(len(r) for r in files.values())
    for name, recs in sorted(files.items()):
        print(f"{name}: {len(recs)}")
    print(f"total: {total}")
