"""Rewrites the expected results of the column sqllogictest suite (#680)
from DuckDB, the column engine's reference (ADR 0000 (f), ADR 0030).

Run via `make refresh-column-sqllogictest`, which pins the DuckDB version
through uv. Local only: CI runs `cargo test --test column_sqllogictest`
against the results this writes, never DuckDB itself.

Every Parquet fixture in tests/fixtures/parquet/ becomes a view named by
its file stem, so a `.test` file's SQL runs unchanged on both engines. For
each `query` record the type string and the expected block are replaced
with DuckDB's answer, rendered as tests/sqllogictest/column.rs renders the
column engine's; a query without a `----` yet gets one. `statement`
records, comments and `skipif` lines are left as they are.

Usage: refresh_column_sqllogictest.py <test-dir> <fixture-dir>
"""

import datetime
import decimal
import math
import pathlib
import sys

import duckdb

INTEGER_TYPES = {
    "TINYINT",
    "SMALLINT",
    "INTEGER",
    "BIGINT",
    "HUGEINT",
    "UTINYINT",
    "USMALLINT",
    "UINTEGER",
    "UBIGINT",
    "UHUGEINT",
    "BOOLEAN",
}


def type_letter(duck_type) -> str:
    name = str(duck_type).upper()
    if name in INTEGER_TYPES:
        return "I"
    if name in ("FLOAT", "DOUBLE"):
        return "R"
    return "T"


def format_real(x: float) -> str:
    """db-core's `value::format_real` (sqlite3's `%!.15g`): 15 significant
    digits, always with a decimal point or an exponent."""
    if math.isnan(x):
        return "NULL"
    if math.isinf(x):
        return "-9.0e+999" if x < 0 else "9.0e+999"
    if x == 0.0:
        return "-0.0" if math.copysign(1.0, x) < 0 else "0.0"
    mantissa, _, exp = f"{x:.15g}".partition("e")
    if "." not in mantissa:
        mantissa += ".0"
    return mantissa + ("e" + exp if exp else "")


def render(value) -> str:
    if value is None:
        return "NULL"
    if isinstance(value, bool):
        return "1" if value else "0"
    if isinstance(value, float):
        return format_real(value)
    if isinstance(value, (int, decimal.Decimal, datetime.datetime, datetime.date)):
        return str(value)
    if value == "":
        return "(empty)"
    return str(value)


def answer(con, sql: str, sort: str) -> tuple[str, list[str]]:
    rel = con.sql(sql)
    types = "".join(type_letter(t) for t in rel.types)
    rows = [[render(v) for v in row] for row in rel.fetchall()]
    if sort == "rowsort":
        rows.sort()
    values = [v for row in rows for v in row]
    if sort == "valuesort":
        values.sort()
    return types, values


def refresh(path: pathlib.Path, con) -> int:
    lines = path.read_text().split("\n")
    out: list[str] = []
    count = 0
    i = 0
    while i < len(lines):
        words = lines[i].split()
        if len(words) != 3 or words[0] != "query":
            out.append(lines[i])
            i += 1
            continue
        header_line = i + 1
        sort = words[2]
        i += 1
        sql_lines = []
        while i < len(lines) and lines[i].strip() and lines[i] != "----":
            sql_lines.append(lines[i])
            i += 1
        if i < len(lines) and lines[i] == "----":
            i += 1
            while i < len(lines) and lines[i].strip():
                i += 1
        sql = "\n".join(sql_lines)
        try:
            types, values = answer(con, sql, sort)
        except duckdb.Error as e:
            sys.exit(
                f"{path.name}:{header_line}: DuckDB refused the query: {e}\n    {sql}"
            )
        out.append(f"query {types} {sort}")
        out.extend(sql_lines)
        out.append("----")
        out.extend(values)
        count += 1
    path.write_text("\n".join(out))
    return count


def main() -> None:
    test_dir, fixture_dir = (pathlib.Path(a) for a in sys.argv[1:3])
    con = duckdb.connect()
    for fixture in sorted(fixture_dir.glob("*.parquet")):
        con.execute(
            f"CREATE VIEW {fixture.stem} AS SELECT * FROM read_parquet('{fixture}')"
        )
    total = 0
    for path in sorted(test_dir.glob("*.test")):
        n = refresh(path, con)
        print(f"{path.name}: {n} queries")
        total += n
    print(f"refreshed {total} queries with DuckDB {duckdb.__version__}")


if __name__ == "__main__":
    main()
