#!/usr/bin/env bash
# Local-only oracle aid (#787, criterion 3 of #600): a row-engine file whose
# rowids lie on both sides of 2^63 stays a file the sqlite3 binary reads.
# A key at or above 2^63 is stored as its signed 64-bit bit pattern, so
# sqlite3 sees `-9223372036854775808` for 2^63 and `-1` for u64::MAX, and
# its signed ORDER BY puts the high keys first -- the opposite of unsigned
# order. Nothing else tests that the file format, rather than our own
# reader, says so.
#
# `examples/row_keys_oracle` writes the table through db-core's writer
# (4,500 rows in three clusters: near u64::MAX, near 0, across 2^63, so the
# b-tree has interior pages) and prints what db-core reads back. This
# script runs the very same queries through the `sqlite3` binary, diffs the
# two, pins the signed values and order, and runs `PRAGMA integrity_check`.
# Run it by hand after a change to `storage::row`'s b-tree or the row
# engine's INSERT path.
#
# Usage: tools/check_row_keys_oracle.sh   (needs `sqlite3` on PATH)
set -euo pipefail

command -v sqlite3 >/dev/null || { echo "check-row-keys-oracle: sqlite3 not found on PATH" >&2; exit 2; }

OUT=target/row-keys-oracle
mkdir -p "$OUT"
rm -f "$OUT/keys.db"
cargo run --quiet --release --example row_keys_oracle -- "$OUT/keys.db" > "$OUT/db-core.txt"

# The same queries, as db-core ran them, through sqlite3.
: > "$OUT/sqlite3.txt"
grep '^-- ' "$OUT/db-core.txt" | while IFS= read -r header; do
  echo "$header" >> "$OUT/sqlite3.txt"
  sqlite3 "$OUT/keys.db" "${header#-- }" >> "$OUT/sqlite3.txt"
done

fail=0
if ! diff -u "$OUT/db-core.txt" "$OUT/sqlite3.txt"; then
  echo "FAIL: db-core and sqlite3 read different rows" >&2
  fail=1
fi

check() { # check <what> <expected> <sqlite3 sql>
  local got
  got="$(sqlite3 "$OUT/keys.db" "$3")"
  if [ "$got" != "$2" ]; then
    echo "FAIL: $1: expected '$2', got '$got'" >&2
    fail=1
  fi
}

check "integrity" "ok" "PRAGMA integrity_check"
check "2^63 is the most negative rowid" \
  "-9223372036854775808|k9223372036854775808" \
  "SELECT id, label FROM keys WHERE id = -9223372036854775808"
check "u64::MAX is rowid -1" \
  "-1|k18446744073709551615" \
  "SELECT id, label FROM keys WHERE id = -1"
check "signed order starts at 2^63, not at 0" \
  "-9223372036854775808" "SELECT min(id) FROM keys"
check "the first row in signed order is 2^63" \
  "-9223372036854775808|k9223372036854775808" \
  "SELECT id, label FROM keys ORDER BY id LIMIT 1"
check "the last row in signed order is 2^63 - 1" \
  "9223372036854775807|k9223372036854775807" \
  "SELECT id, label FROM keys ORDER BY id DESC LIMIT 1"
check "0 follows u64::MAX in signed order" \
  "-1|k18446744073709551615
0|k0" \
  "SELECT id, label FROM keys WHERE id BETWEEN -1 AND 0 ORDER BY id"
check "row count" "4500" "SELECT count(*) FROM keys"

if [ "$fail" -ne 0 ]; then
  exit 1
fi
echo "check-row-keys-oracle: db-core's file reads as signed keys in sqlite3 $(sqlite3 --version | cut -d' ' -f1)"
