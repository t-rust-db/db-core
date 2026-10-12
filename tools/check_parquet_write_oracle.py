"""Reads the Parquet writer's interoperability cases (#566) with DuckDB and
pyarrow and checks them against the values that were written.

Each `<case>.parquet` comes with a `<case>.json` (from
`examples/parquet_write_oracle.rs`) holding the schema and the exact input
values. For both readers this checks, per case:

- the row count and column types,
- every value (floats bit-for-bit up to NaN, so -0.0 counts; FLOAT values
  rounded to f32 as written),
- per row group: the null count, and min/max equal to the bounds computed
  here from the input slice (NaN ignored, a zero min reported as -0.0 and a
  zero max as +0.0), or no min/max when the slice has no non-NaN value.

Usage: make check-parquet-write-oracle
   or: uv run --with duckdb==V --with pyarrow==V python tools/check_parquet_write_oracle.py <dir>
"""

import json
import math
import struct
import sys
from pathlib import Path

import duckdb
import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq

ARROW_TYPES = {
    "boolean": pa.bool_(),
    "int32": pa.int32(),
    "int64": pa.int64(),
    "float": pa.float32(),
    "double": pa.float64(),
    "utf8": pa.string(),
    "timestamp_micros": pa.timestamp("us", tz="UTC"),
    "timestamp_nanos": pa.timestamp("ns"),
}

DUCKDB_TYPES = {
    "boolean": "BOOLEAN",
    "int32": "INTEGER",
    "int64": "BIGINT",
    "float": "FLOAT",
    "double": "DOUBLE",
    "utf8": "VARCHAR",
    "timestamp_micros": "TIMESTAMP WITH TIME ZONE",
    "timestamp_nanos": "TIMESTAMP_NS",
}

# How DuckDB's VARCHAR stats_min_value/stats_max_value are cast back.
DUCKDB_STAT_CAST = {
    "boolean": "{}::BOOLEAN",
    "int32": "{}::INTEGER",
    "int64": "{}::BIGINT",
    "float": "{}::FLOAT",
    "double": "{}::DOUBLE",
    "utf8": "{}",
    "timestamp_micros": "epoch_us({}::TIMESTAMPTZ)",
    "timestamp_nanos": "epoch_ns({}::TIMESTAMP_NS)",
}


def f32(x):
    return struct.unpack("<f", struct.pack("<f", x))[0]


def normalize(column_type, value):
    """A value as both sides should compare it."""
    if value is None:
        return None
    if column_type == "float":
        value = f32(value)
    if column_type in ("float", "double"):
        if math.isnan(value):
            return "NaN"
        return (value, math.copysign(1.0, value))
    if column_type == "utf8" and isinstance(value, bytes):
        return value.decode("utf-8")
    return value


def expected_bounds(column_type, values):
    """(null_count, min, max) for one row group slice; min/max None when
    there is no non-null, non-NaN value."""
    nulls = sum(v is None for v in values)
    present = [v for v in values if v is not None]
    if column_type in ("float", "double"):
        present = [f32(v) if column_type == "float" else v for v in present]
        present = [v for v in present if not math.isnan(v)]
    if not present:
        return nulls, None, None
    lo, hi = min(present), max(present)
    if column_type in ("float", "double"):
        lo = -0.0 if lo == 0 else lo
        hi = 0.0 if hi == 0 else hi
    return nulls, normalize(column_type, lo), normalize(column_type, hi)


class Failures:
    def __init__(self):
        self.items = []

    def check(self, ok, what):
        if not ok:
            self.items.append(what)


def check_pyarrow(path, columns, fails):
    table = pq.read_table(path)
    meta = pq.ParquetFile(path).metadata
    expected_rows = next(len(c["values"]) for c in columns if c["values"] is not None)
    fails.check(table.num_rows == expected_rows, f"pyarrow: {table.num_rows} rows, expected {expected_rows}")
    for i, col in enumerate(columns):
        name, ctype = col["name"], col["type"]
        field = table.schema.field(name)
        fails.check(field.type == ARROW_TYPES[ctype], f"pyarrow {name}: type {field.type}")
        fails.check(field.nullable == col["nullable"], f"pyarrow {name}: nullable {field.nullable}")
        data = table.column(name)
        if ctype in ("timestamp_micros", "timestamp_nanos"):
            data = pc.cast(data, pa.int64())
        got = [normalize(ctype, v) for v in data.to_pylist()]
        if col["values"] is None:
            # Not derivable from the input (see the example): pyarrow's
            # reading is the reference DuckDB and the statistics must match.
            col["values"] = [v for v in data.to_pylist()]
        want = [normalize(ctype, v) for v in col["values"]]
        fails.check(got == want, f"pyarrow {name}: values differ (first at {first_diff(got, want)})")
        start = 0
        for rg in range(meta.num_row_groups):
            n = meta.row_group(rg).num_rows
            nulls, lo, hi = expected_bounds(ctype, col["values"][start : start + n])
            stats = meta.row_group(rg).column(i).statistics
            where = f"pyarrow {name} rg{rg}"
            fails.check(stats is not None and stats.null_count == nulls, f"{where}: null_count {stats and stats.null_count}, expected {nulls}")
            if lo is None:
                fails.check(stats is None or not stats.has_min_max, f"{where}: has min/max, expected none")
            else:
                got_lo = normalize(ctype, stats.min_raw) if stats and stats.has_min_max else None
                got_hi = normalize(ctype, stats.max_raw) if stats and stats.has_min_max else None
                fails.check((got_lo, got_hi) == (lo, hi), f"{where}: bounds {got_lo, got_hi}, expected {lo, hi}")
            start += n


def check_duckdb(path, columns, fails):
    con = duckdb.connect()
    p = str(path).replace("'", "''")
    expected_rows = len(columns[0]["values"])
    (rows,) = con.execute(f"SELECT count(*) FROM read_parquet('{p}')").fetchone()
    fails.check(rows == expected_rows, f"duckdb: {rows} rows, expected {expected_rows}")
    types = dict(con.execute(f"SELECT column_name, column_type FROM (DESCRIBE SELECT * FROM read_parquet('{p}'))").fetchall())
    groups = con.execute(
        f"SELECT DISTINCT row_group_id, row_group_num_rows FROM parquet_metadata('{p}') ORDER BY 1"
    ).fetchall()
    for col in columns:
        name, ctype = col["name"], col["type"]
        q = '"' + name.replace('"', '""') + '"'
        fails.check(types.get(name) == DUCKDB_TYPES[ctype], f"duckdb {name}: type {types.get(name)}")
        select = {"timestamp_micros": f"epoch_us({q})", "timestamp_nanos": f"epoch_ns({q})"}.get(ctype, q)
        got = [normalize(ctype, r[0]) for r in con.execute(f"SELECT {select} FROM read_parquet('{p}')").fetchall()]
        want = [normalize(ctype, v) for v in col["values"]]
        fails.check(got == want, f"duckdb {name}: values differ (first at {first_diff(got, want)})")
        cast = DUCKDB_STAT_CAST[ctype]
        stats = con.execute(
            f"SELECT row_group_id, stats_null_count, {cast.format('stats_min_value')}, {cast.format('stats_max_value')} "
            f"FROM parquet_metadata('{p}') WHERE path_in_schema = ? ORDER BY row_group_id",
            [name],
        ).fetchall()
        start = 0
        for (rg, n), (_, null_count, lo_got, hi_got) in zip(groups, stats):
            nulls, lo, hi = expected_bounds(ctype, col["values"][start : start + n])
            where = f"duckdb {name} rg{rg}"
            fails.check(null_count == nulls, f"{where}: null_count {null_count}, expected {nulls}")
            got = (normalize(ctype, lo_got), normalize(ctype, hi_got))
            # DuckDB reports no float bounds when one is infinite, for files it
            # writes itself too (COPY ... TO with 'inf' gives NULL stats).
            infinite = ctype in ("float", "double") and lo is not None and (math.isinf(lo[0]) or math.isinf(hi[0]))
            if infinite and got == (None, None):
                pass
            else:
                fails.check(got == (lo, hi), f"{where}: bounds {got}, expected {lo, hi}")
            start += n
        fails.check(len(stats) == len(groups), f"duckdb {name}: {len(stats)} chunks, {len(groups)} row groups")


def check_archive(spec, fails):
    """#573: DuckDB over a tiered table's `archive/` returns exactly the
    rows of its closed days, each under its `date` partition."""
    expected = sorted((date, raw) for date, raw in json.loads(spec.read_text()))
    table = spec.with_suffix("")
    glob = (table / "archive" / "*" / "*.parquet").as_posix()
    con = duckdb.connect()
    got = con.execute(
        f"SELECT CAST(date AS VARCHAR), raw FROM read_parquet('{glob}', "
        "hive_partitioning = true, union_by_name = true) ORDER BY raw"
    ).fetchall()
    got = sorted((d, r) for d, r in got)
    if got != expected:
        fails.add(f"archive: {len(got)} rows vs {len(expected)} expected; {first_diff(got, expected)}")
    # The open day is in live/ only, never in the archive.
    live = (table / "live").as_posix()
    if not any(Path(live).rglob("*.parquet")):
        fails.add("live/: expected the open day's hours")


def first_diff(a, b):
    for i, (x, y) in enumerate(zip(a, b)):
        if x != y:
            return f"{i}: {x!r} != {y!r}"
    return f"length {len(a)} vs {len(b)}"


def main():
    if len(sys.argv) != 2:
        sys.exit("usage: check_parquet_write_oracle.py <dir>")
    cases = sorted(Path(sys.argv[1]).glob("*.json"))
    if not cases:
        sys.exit(f"no cases in {sys.argv[1]}")
    print(f"duckdb {duckdb.__version__}, pyarrow {pa.__version__}")
    failed = 0
    for case in cases:
        columns = json.loads(case.read_text())["columns"]
        path = case.with_suffix(".parquet")
        fails = Failures()
        check_pyarrow(path, columns, fails)
        check_duckdb(path, columns, fails)
        status = "ok" if not fails.items else f"FAIL ({len(fails.items)})"
        print(f"  {case.stem}: {status}")
        for item in fails.items[:20]:
            print(f"    {item}")
        failed += bool(fails.items)
    archives = sorted(Path(sys.argv[1]).glob("*.archive"))
    for spec in archives:
        fails = Failures()
        check_archive(spec, fails)
        status = "ok" if not fails.items else f"FAIL ({len(fails.items)})"
        print(f"  {spec.stem} (archive/): {status}")
        for item in fails.items[:20]:
            print(f"    {item}")
        failed += bool(fails.items)
    if failed:
        sys.exit(f"{failed} of {len(cases) + len(archives)} cases failed")
    print(f"all {len(cases) + len(archives)} cases agree with DuckDB and pyarrow")


if __name__ == "__main__":
    main()
