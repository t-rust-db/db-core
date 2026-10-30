#!/usr/bin/env bash
# The high-volume event workload (#602): db-core's tiered record table
# against a row store (SQLite, three indexes) and DuckDB over the same
# Parquet files. Local only and report only, like check_column_oracle.sh:
# it needs the `sqlite3` and `duckdb` CLIs on PATH and never runs in CI.
#
# Every step runs as its own process under /usr/bin/time, which gives its
# wall time and peak memory. Each query runs on all three engines and the
# results are compared (sorted rows): a db-core or SQLite answer that
# differs from DuckDB's fails the run.
#
# Usage: tools/bench_events.sh   (EVENTS=10000000 DAYS=7 SEED=42
# SYNC_EVERY=10000 CODEC=snappy by default; EVENTS=200000 for a quick
# run; CODEC=zstd or uncompressed for the page codec, #622;
# ROW_GROUP_MAX_BYTES / SPILL_BYTES set the byte budgets, #854; REQUESTS=300000
# sizes the request-shaped wide workload;
# STAT_COLUMNS=day by default, empty for none; SESSION_KEY_SECS=14400 turns
# on session_key with timestamped session ids, #599)
set -euo pipefail

export EVENTS="${EVENTS:-10000000}" DAYS="${DAYS:-7}" SEED="${SEED:-42}" SYNC_EVERY="${SYNC_EVERY:-10000}"
OUT=target/perf/events
REPORT_MD=target/perf/bench_events.md
REPORT_JSON=target/perf/bench_events.json
rm -rf "$OUT"
mkdir -p "$OUT"
TABLE="$OUT/events"
SQLITE_DB="$OUT/events.sqlite"
CSV="$OUT/events.csv"
RESULTS="$OUT/results.tsv"
: >"$RESULTS"

for tool in sqlite3 duckdb python3; do
  command -v "$tool" >/dev/null || { echo "bench-events: needs $tool on PATH"; exit 1; }
done

echo "building benches/events.rs (release)..."
# The bench target's executable, by name: the build can emit other binaries
# (the ingest probes), and the last executable is not necessarily this one.
BIN="$(cargo bench --bench events --no-run --message-format=json 2>/dev/null |
  python3 -c 'import json,sys
for line in sys.stdin:
    j = json.loads(line)
    if j.get("executable") and j.get("target", {}).get("name") == "events": print(j["executable"])' | tail -1)"

if /usr/bin/time -l true 2>/dev/null; then TIME_FLAG=-l; else TIME_FLAG=-v; fi

# timed LABEL CMD...: stdout to $OUT/LABEL.out, stderr (with time's
# report) to $OUT/LABEL.err; appends "LABEL<TAB>wall_s<TAB>peak_rss_mb".
timed() {
  local label="$1"
  shift
  /usr/bin/time "$TIME_FLAG" "$@" >"$OUT/$label.out" 2>"$OUT/$label.err" || {
    echo "bench-events: $label failed:"
    tail -5 "$OUT/$label.err"
    exit 1
  }
  python3 - "$OUT/$label.err" "$label" >>"$RESULTS" <<'PY'
import re, sys
text = open(sys.argv[1]).read()
if m := re.search(r"([\d.]+) real", text):          # macOS
    wall = float(m.group(1))
    rss = int(re.search(r"(\d+)\s+maximum resident set size", text).group(1)) / 2**20
else:                                                # GNU time -v
    h, m_, s = (["0"] + re.search(r"Elapsed \(wall clock\) time.*: ([\d:.]+)", text).group(1).split(":"))[-3:]
    wall = int(h) * 3600 + int(m_) * 60 + float(s)
    rss = int(re.search(r"Maximum resident set size \(kbytes\): (\d+)", text).group(1)) / 1024
print(f"{sys.argv[2]}\t{wall:.3f}\t{rss:.1f}")
PY
}

echo "db-core: ingest $EVENTS events over $DAYS days..."
timed ingest_dbcore "$BIN" ingest "$TABLE"
"$BIN" storage "$TABLE" >"$OUT/storage_dbcore.out"
FILES="$("$BIN" files "$TABLE" | sed "s/.*/'&'/" | paste -sd, -)"
DUCK_VIEW="CREATE VIEW events AS SELECT * FROM read_parquet([$FILES]);"

echo "db-core: ingest ${REQUESTS:-300000} wide requests (24 columns, 4 MAPs, 2 BINARYs)..."
timed ingest_wide "$BIN" wide "$OUT/wide"

echo "SQLite: load the same events and build three indexes..."
"$BIN" csv "$CSV"
cat >"$OUT/sqlite_load.sql" <<SQL
CREATE TABLE events(session_id INTEGER NOT NULL, request_id INTEGER NOT NULL,
  ts INTEGER NOT NULL, day INTEGER NOT NULL, name TEXT NOT NULL,
  user_id INTEGER NOT NULL, amount INTEGER);
.import --csv --skip 1 $CSV events
UPDATE events SET amount = NULL WHERE amount = '';
CREATE INDEX events_session ON events(session_id, request_id);
CREATE INDEX events_day_name ON events(day, name);
CREATE INDEX events_name_session ON events(name, session_id);
SQL
timed ingest_sqlite sqlite3 "$SQLITE_DB" ".read $OUT/sqlite_load.sql"
rm -f "$CSV"

# The queries. The row store and DuckDB run the same SQL so the answers
# compare, and every result is compared as sorted rows.
S=$("$BIN" sid $((EVENTS / 40)))
N=1000
LABELS=(stats replay session cross oneday)
# One function, not an associative array: macOS's /bin/bash is 3.2.
sql_for() {
  case "$1" in
    stats) echo "SELECT day, name, count(*), sum(amount) FROM events GROUP BY day, name" ;;
    replay) echo "SELECT session_id, request_id, name FROM events WHERE session_id >= $S AND session_id < $((S + N)) ORDER BY session_id, request_id" ;;
    session) echo "SELECT request_id, name, amount FROM events WHERE session_id = $S" ;;
    oneday) echo "SELECT name, count(*), sum(amount) FROM events WHERE day = 20740 GROUP BY name" ;;
    cross) echo "SELECT session_id, count(DISTINCT name) FROM events WHERE name = 'search' OR name = 'checkout' GROUP BY session_id HAVING count(DISTINCT name) = 2" ;;
  esac
}

fail=0
for q in "${LABELS[@]}"; do
  sql="$(sql_for "$q")"
  echo "query $q..."
  timed "q_${q}_dbcore" "$BIN" query "$TABLE" "$sql"
  timed "q_${q}_duckdb" duckdb -csv -noheader -c "$DUCK_VIEW $sql"
  timed "q_${q}_sqlite" sqlite3 -csv -cmd ".nullvalue NULL" "$SQLITE_DB" "$sql"
  for engine in dbcore duckdb sqlite; do
    f="$OUT/q_${q}_${engine}.out"
    # sqlite3 3.53's -csv ends lines with CRLF (RFC 4180); the others LF.
    tr -d '\r' <"$f" >"$f.tmp" && mv "$f.tmp" "$f"
    LC_ALL=C sort "$f" -o "$f"
  done
  for engine in dbcore sqlite; do
    if ! cmp -s "$OUT/q_${q}_${engine}.out" "$OUT/q_${q}_duckdb.out"; then
      echo "MISMATCH: $q: $engine differs from DuckDB ($OUT/q_${q}_${engine}.out)"
      fail=1
    fi
  done
done

# A join build side or window input, read whole and filtered (#777).
echo "build sides..."
"$BIN" build "$TABLE" "session_id = $S" >"$OUT/build_session.out"
"$BIN" build "$TABLE" "day = 20740" >"$OUT/build_day.out"

# A fresh read of a big unflushed tail, opened anew and incrementally (#593).
echo "tail reader..."
TAIL="$(((EVENTS - 3000) / 2 < 1000000 ? (EVENTS - 3000) / 2 : 1000000))" "$BIN" tail "$OUT/tail" >"$OUT/tail.out"

python3 - "$RESULTS" "$OUT" "$REPORT_MD" "$REPORT_JSON" "$SQLITE_DB" <<'PY'
import json, os, sys
results_path, out, md_path, json_path, sqlite_db = sys.argv[1:]
res = {}
for line in open(results_path):
    label, wall, rss = line.rstrip("\n").split("\t")
    res[label] = {"wall_s": float(wall), "peak_rss_mb": float(rss)}
def kv(path):
    return dict(l.strip().split("=", 1) for l in open(path) if "=" in l)
ingest = kv(f"{out}/ingest_dbcore.out")
storage = kv(f"{out}/storage_dbcore.out")
events = int(ingest["events"])
sqlite_bytes = os.path.getsize(sqlite_db)
report = {
    "events": events,
    "days": int(os.environ["DAYS"]),
    "seed": int(os.environ["SEED"]),
    "ingest": {
        "dbcore": {**{k: float(v) for k, v in ingest.items()}, **res["ingest_dbcore"]},
        "dbcore_wide": {**{k: float(v) for k, v in kv(f"{out}/ingest_wide.out").items()}, **res["ingest_wide"]},
        "sqlite_bulk_load_and_index": res["ingest_sqlite"],
    },
    "storage_bytes_per_event": {
        "dbcore_parquet": float(storage["bytes_per_event"]),
        "duckdb_same_parquet": float(storage["bytes_per_event"]),
        "sqlite_with_indexes": sqlite_bytes / events,
    },
    "queries": {},
}
for q in ["stats", "replay", "session", "cross", "oneday"]:
    rows = sum(1 for _ in open(f"{out}/q_{q}_duckdb.out"))
    entry = {"rows": rows}
    for engine in ["dbcore", "duckdb", "sqlite"]:
        entry[engine] = res[f"q_{q}_{engine}"]
    entry["dbcore"]["in_process_ms"] = float(kv(f"{out}/q_{q}_dbcore.err")["elapsed_ms"])
    err = kv(f"{out}/q_{q}_dbcore.err")
    entry["dbcore"]["files_opened"] = int(err["files_opened"])
    entry["dbcore"]["files_total"] = int(err["files_total"])
    report["queries"][q] = entry
report["build_side"] = {
    name: {k: float(v) for k, v in kv(f"{out}/build_{name}.out").items()}
    for name in ["session", "day"]
}
report["tail_reader"] = {k: float(v) for k, v in kv(f"{out}/tail.out").items()}
json.dump(report, open(json_path, "w"), indent=2)

w = report["ingest"]["dbcore_wide"]
i, d = report["ingest"]["dbcore"], report["ingest"]["sqlite_bulk_load_and_index"]
s = report["storage_bytes_per_event"]
lines = [
    f"# Event workload (#602): {events:,} events over {report['days']} days, seed {report['seed']}",
    "",
    "## Ingest",
    "",
    "| | wall s | events/s | peak RSS MB |",
    "|---|---:|---:|---:|",
    f"| db-core: append + group commit (sync every {os.environ['SYNC_EVERY']}) | {i['ingest_s']:.2f} | {i['events_per_s']:,.0f} | |",
    f"| db-core: of which hour flushes | {i['hour_flush_s']:.2f} | | |",
    f"| db-core: day merge (compact) | {i['day_compact_s']:.2f} | | |",
    f"| db-core: whole process | {i['wall_s']:.2f} | | {i['peak_rss_mb']:.0f} |",
    f"| SQLite: bulk CSV import + 3 indexes | {d['wall_s']:.2f} | {events / d['wall_s']:,.0f} | {d['peak_rss_mb']:.0f} |",
    "",
    "## Peak memory per phase, MB (#854)",
    "",
    "| workload | open hour (append) | hour flush | day merge | whole process |",
    "|---|---:|---:|---:|---:|",
    f"| events ({events:,}, 7 columns) | {i['peak_append_mb']:.0f} | {i['peak_flush_mb']:.0f} | {i['peak_merge_mb']:.0f} | {i['peak_rss_mb']:.0f} |",
    f"| requests ({w['events']:,.0f}, 24 columns, 4 MAPs + 2 BINARYs) | {w['peak_append_mb']:.0f} | {w['peak_flush_mb']:.0f} | {w['peak_merge_mb']:.0f} | {w['peak_rss_mb']:.0f} |",
    "",
    "## Storage",
    "",
    "| | bytes/event |",
    "|---|---:|",
    f"| db-core Parquet (DuckDB reads the same files) | {s['dbcore_parquet']:.1f} |",
    f"| SQLite with 3 indexes | {s['sqlite_with_indexes']:.1f} |",
    "",
    "## Queries (wall s per process / peak RSS MB; db-core in-process ms)",
    "",
    "| query | rows | db-core | db-core in-process ms | db-core files opened | DuckDB | SQLite |",
    "|---|---:|---:|---:|---:|---:|---:|",
]
for q, e in report["queries"].items():
    cell = lambda x: f"{x['wall_s']:.2f} / {x['peak_rss_mb']:.0f}"
    lines.append(f"| {q} | {e['rows']:,} | {cell(e['dbcore'])} | {e['dbcore']['in_process_ms']:.0f} | {e['dbcore']['files_opened']}/{e['dbcore']['files_total']} | {cell(e['duckdb'])} | {cell(e['sqlite'])} |")
lines += [
    "",
    "## Join build side / window input (`read_snapshot_where`, #777)",
    "",
    "| predicate | rows read (all -> filtered) | files opened | ms (all -> filtered) |",
    "|---|---:|---:|---:|",
]
for name, b in report["build_side"].items():
    lines.append(f"| {name} | {b['rows_all']:,.0f} -> {b['rows_filtered']:,.0f} | {b['files_opened']:.0f}/{b['files_total']:.0f} | {b['all_ms']:.0f} -> {b['filtered_ms']:.0f} |")
t = report["tail_reader"]
lines += [
    "",
    f"## Reader in another process, {t['tail_records']:,.0f} records in the WAL (#593)",
    "",
    "| | fresh `Snapshot::open` ms | long-lived `TableReader` ms |",
    "|---|---:|---:|",
    f"| first read | {t['fresh_open_ms']:.0f} | {t['reader_open_ms']:.0f} |",
    f"| after 1,000 more records | {t['fresh_after_more_ms']:.0f} | {t['refresh_after_more_ms']:.1f} |",
    f"| after the first hour flushed (a second hour's tail left), and 1,000 more | {t['fresh_after_flush_ms']:.0f} | {t['refresh_after_flush_ms']:.1f} |",
]
open(md_path, "w").write("\n".join(lines) + "\n")
print("\n".join(lines))
PY

if [[ "$fail" -ne 0 ]]; then
  echo "bench-events: FAILED (results differ)"
  exit 1
fi
echo "bench-events: ok -- every query agrees with DuckDB; report in $REPORT_MD and $REPORT_JSON"
