// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! A fixed adversarial suite (#877): hostile SQL *values* and hostile
//! *files*, the inputs the structural nesting suite (#226) and the grammar
//! fuzzer (#543) never produce -- huge lengths, i64 edges, pathological
//! patterns, long chains, bad bytes, corrupt pages, bad WAL and journal
//! files. Each SQL row runs through the row engine on a **2 MiB thread**
//! (an embedding application's worker thread; the 8 MiB main thread hides
//! stack overflows) and asserts today's answer: rows, an error, or just
//! that it finished. A panic, an abort or a stack overflow fails the suite,
//! and so does a row that takes longer than [`TIME_LIMIT`].
//!
//! The SQL rows are the 62 probes of the 2026-10-30 stability review
//! (#879); fuzz findings (#543) get promoted into rows here. An allocation
//! failure aborts the process and cannot be caught, so the rows that ask
//! for huge sizes rely on the size guards firing before the allocation.
//!
//! `DB_CORE_ADVERSARIAL_CAPTURE=1` prints every row's actual outcome and
//! time instead of asserting -- how the expectations below were recorded.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use db_core::engine::row::RowEngine;
use db_core::engine::Engine;

/// The longest any row may take, in the debug build CI runs.
const TIME_LIMIT: Duration = Duration::from_secs(5);

const FIXTURE: &str = "tests/fixtures/btrees/table_single_page.db";

/// What a row must do.
enum Expect {
    /// Succeed with these rows: cells joined by `,`, rows by ` | `.
    Rows(&'static str),
    /// Fail with an error whose text contains this.
    Error(&'static str),
}

/// A scratch copy of the row fixture, removed on drop.
struct TempDb(PathBuf);

impl TempDb {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "db-core-adversarial-{label}-{}.db",
            std::process::id()
        ));
        std::fs::copy(FIXTURE, &path).unwrap();
        TempDb(path)
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-journal", "-shm"] {
            let mut p = self.0.clone().into_os_string();
            p.push(suffix);
            std::fs::remove_file(p).ok();
        }
    }
}

/// `sql`'s outcome on a fresh engine over `db`, on a 2 MiB thread: the
/// rows or the error text, and how long it took. A panic in the engine
/// (a stack overflow aborts the whole process) fails here.
fn run(db: &Path, sql: String) -> (Result<String, String>, Duration) {
    let db = db.to_path_buf();
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || {
            let start = Instant::now();
            let outcome = RowEngine::open(&db)
                .map_err(|e| e.to_string())
                .and_then(|mut e| e.run_query(&sql).map_err(|e| e.to_string()))
                .map(|r| {
                    r.rows
                        .iter()
                        .map(|row| {
                            row.iter()
                                .map(ToString::to_string)
                                .collect::<Vec<_>>()
                                .join(",")
                        })
                        .collect::<Vec<_>>()
                        .join(" | ")
                });
            (outcome, start.elapsed())
        })
        .unwrap()
        .join()
        .expect("the engine panicked")
}

/// `SELECT 1 FROM (SELECT 1) t0, (SELECT 1) t1, ...`: `n` tables.
fn joined(n: usize) -> String {
    let tables: Vec<String> = (0..n).map(|i| format!("(SELECT 1) t{i}")).collect();
    format!("SELECT 1 FROM {}", tables.join(","))
}

fn repeat_join(item: &str, n: usize, sep: &str) -> String {
    vec![item; n].join(sep)
}

/// The 62 review probes, and the rows this suite has added since: name,
/// SQL, expected outcome.
fn sql_rows() -> Vec<(&'static str, String, Expect)> {
    use Expect::*;
    let deep_json = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
    vec![
        // Sizes: every guard fires before the allocation.
        ("zeroblob_huge", "SELECT length(zeroblob(1000000000000))".into(), Error("string or blob too big")),
        ("zeroblob_i64max", "SELECT length(zeroblob(9223372036854775807))".into(), Error("string or blob too big")),
        ("randomblob_huge", "SELECT length(randomblob(1000000000000))".into(), Error("string or blob too big")),
        ("printf_width", "SELECT length(printf('%2000000000d', 1))".into(), Error("string or blob too big")),
        ("printf_prec", "SELECT length(printf('%.2000000000f', 1.0))".into(), Rows("100000002")),
        ("printf_star", "SELECT length(printf('%*d', 2147483647, 1))".into(), Error("string or blob too big")),
        ("format_width", "SELECT length(format('%9223372036854775807d', 1))".into(), Error("string or blob too big")),
        ("substr_huge", "SELECT substr('abc', -9223372036854775808, 9223372036854775807)".into(), Rows("ab")),
        ("substr_min", "SELECT substr('abc', 9223372036854775807, -9223372036854775808)".into(), Rows("abc")),
        ("char_huge", "SELECT char(1114112, -1, 9223372036854775807)".into(), Rows("���")),
        ("hex_zb", "SELECT length(hex(zeroblob(2000000000)))".into(), Error("string or blob too big")),
        (
            "replace_bomb",
            "SELECT length(replace(replace(replace(replace(replace('aaaaaaaaaa','a','aaaaaaaaaa'),'a','aaaaaaaaaa'),'a','aaaaaaaaaa'),'a','aaaaaaaaaa'),'a','aaaaaaaaaa'))".into(),
            Rows("1000000"),
        ),
        // Integer and real edges.
        ("round_digits", "SELECT round(1.5, 9223372036854775807), round(1.5, -9223372036854775808)".into(), Rows("1.5000000000000002,2.0")),
        ("abs_min", "SELECT abs(-9223372036854775808)".into(), Error("integer overflow")),
        ("neg_min", "SELECT -(-9223372036854775808)".into(), Rows("9.2233720368547758e+18")),
        ("div_min", "SELECT (-9223372036854775808) / -1, (-9223372036854775808) % -1".into(), Rows("9.2233720368547758e+18,0")),
        ("sum_overflow", "SELECT sum(x) FROM (SELECT 9223372036854775807 AS x UNION ALL SELECT 1)".into(), Error("integer overflow")),
        ("shift", "SELECT 1 << 9223372036854775807, 1 >> -9223372036854775808, 1 << -64".into(), Rows("0,0,0")),
        ("cast_huge_real", "SELECT CAST(1e400 AS INTEGER), CAST(-1e400 AS INTEGER), CAST('nan' AS REAL)".into(), Rows("9223372036854775807,-9223372036854775808,0.0")),
        // Patterns.
        ("like_patho", format!("SELECT '{}' LIKE '{}b'", "a".repeat(30), "%a".repeat(30)), Rows("0")),
        ("glob_patho", format!("SELECT '{}' GLOB '{}b'", "a".repeat(30), "*a".repeat(30)), Rows("0")),
        ("like_escape_bad", "SELECT 'a' LIKE 'a' ESCAPE 'xy'".into(), Error("ESCAPE expression must be a single character")),
        ("like_huge", format!("SELECT '{}' LIKE '{}b'", "a".repeat(100_000), "%".repeat(50_000)), Error("LIKE or GLOB pattern too complex")),
        // Long chains and deep nesting.
        ("compound_chain", format!("SELECT 1{}", " UNION SELECT 1".repeat(20_000)), Error("unsupported: a FROM-less SELECT only supports a bare expression list —")),
        ("compound_chain_all", format!("SELECT 1{}", " UNION ALL SELECT 1".repeat(20_000)), Error("unsupported: a FROM-less SELECT only supports a bare expression list —")),
        ("subquery_nest", format!("SELECT {}1{}", "(SELECT ".repeat(5_000), ")".repeat(5_000)), Error("subquery nesting too deep")),
        (
            "from_subq_nest",
            format!("SELECT * FROM {}(SELECT 1){}", "(SELECT * FROM ".repeat(3_000), ")".repeat(3_000)),
            Error("subquery nesting too deep"),
        ),
        ("in_list_long", format!("SELECT 1 IN ({})", repeat_join("1", 200_000, ",")), Rows("1")),
        (
            "case_long",
            format!(
                "SELECT CASE 1 {} END",
                (0..100_000).map(|i| format!("WHEN {i} THEN {i}")).collect::<Vec<_>>().join(" ")
            ),
            Rows("1"),
        ),
        ("and_chain", format!("SELECT 1 WHERE {}", repeat_join("1=1", 100_000, " AND ")), Error("Expression tree is too large (maximum depth 1000)")),
        ("concat_chain", format!("SELECT {}", repeat_join("'a'", 100_000, " || ")), Error("Expression tree is too large (maximum depth 1000)")),
        ("plus_chain", format!("SELECT {}", repeat_join("1", 100_000, "+")), Error("Expression tree is too large (maximum depth 1000)")),
        ("func_args", format!("SELECT max({})", repeat_join("1", 100_000, ",")), Rows("1")),
        (
            "cte_chain",
            format!(
                "WITH {} SELECT * FROM c2999",
                (0..3_000)
                    .map(|i| if i == 0 { "c0 AS (SELECT 1)".to_string() } else { format!("c{i} AS (SELECT * FROM c{})", i - 1) })
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Error("CTEs and views expand to more than 262144 bytes of SQL or 250 levels o"),
        ),
        ("join_many", joined(200), Error("at most 64 tables in a join")),
        // Found by this suite (#877): sqlite3 joins at most 64 tables; past
        // that the row planner's per-table recursion overflowed 2 MiB.
        ("join_64", joined(64), Rows("1")),
        ("join_3000", joined(3_000), Error("at most 64 tables in a join")),
        ("values_wide", format!("VALUES ({})", repeat_join("1", 50_000, ",")), Error("unsupported or unrecognized statement: \"VALUES\" ...")),
        ("values_many", format!("VALUES {}", repeat_join("(1)", 200_000, ",")), Error("unsupported or unrecognized statement: \"VALUES\" ...")),
        // JSON.
        ("json_deep", format!("SELECT json('{deep_json}')"), Error("malformed JSON")),
        ("json_valid_deep", format!("SELECT json_valid('{deep_json}')"), Rows("0")),
        ("json_extract_deep", format!("SELECT json_extract('{deep_json}', '$[0]')"), Error("malformed JSON")),
        ("json_each_deep", format!("SELECT count(*) FROM json_tree('{deep_json}')"), Error("table-valued functions in FROM not yet supported")),
        (
            "json_path_huge_idx",
            "SELECT json_extract('[1]', '$[9223372036854775807]'), json_extract('[1]','$[#-9223372036854775808]')".into(),
            Rows(","),
        ),
        ("json_set_huge_idx", "SELECT json_set('[]', '$[#+9223372036854775807]', 1)".into(), Error("bad JSON path: '$[#+9223372036854775807]'")),
        // Dates.
        (
            "datetime_mods",
            "SELECT datetime('now', '+9223372036854775807 days'), date('2000-01-01','-9223372036854775808 months'), julianday(9e300)".into(),
            Rows(",,"),
        ),
        (
            "strftime_bad",
            "SELECT strftime('%', 'now'), strftime('%J%s%f', 1e300), datetime(-1e18,'unixepoch')".into(),
            Rows(",,"),
        ),
        // Text and bytes.
        (
            "unicode_slice",
            "SELECT substr('héllo€😀', 2, 3), instr('😀😀','😀'), upper('ß'), trim('😀a😀','😀')".into(),
            Rows("éll,1,ß,a"),
        ),
        ("blob_text_cast", "SELECT CAST(x'fffe' AS TEXT)".into(), Rows("��")),
        (
            "invalid_utf8",
            "SELECT length(CAST(x'c3' AS TEXT)), upper(CAST(x'80ff' AS TEXT)), substr(CAST(x'e282' AS TEXT),1,1)".into(),
            Rows("1,��,�"),
        ),
        ("group_concat_sep", "SELECT group_concat(x, zeroblob(100)) FROM (SELECT 1 x UNION SELECT 2)".into(), Error("unsupported: a FROM-less subquery in FROM only supports bare expressio")),
        // Clauses with edge values.
        ("limit_neg", "SELECT 1 LIMIT -9223372036854775808 OFFSET 9223372036854775807".into(), Error("unsupported: a FROM-less SELECT only supports a bare expression list —")),
        ("order_by_huge", "SELECT 1 ORDER BY 9223372036854775807".into(), Error("unsupported: a FROM-less SELECT only supports a bare expression list —")),
        ("group_by_idx0", "SELECT 1 GROUP BY 0".into(), Error("unsupported: a FROM-less SELECT only supports a bare expression list —")),
        (
            "recursive_cte_limit",
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r) SELECT n FROM r LIMIT 5".into(),
            Error("WITH RECURSIVE not yet supported"),
        ),
        (
            "window_frame_huge",
            "SELECT sum(1) OVER (ROWS BETWEEN 9223372036854775807 PRECEDING AND 9223372036854775807 FOLLOWING)".into(),
            Error("window frame (ROWS/RANGE/GROUPS) not yet supported"),
        ),
        (
            "ntile_zero",
            "SELECT ntile(0) OVER (), nth_value(1, 0) OVER (), lag(1, -9223372036854775808) OVER ()".into(),
            Error("unsupported: window functions (OVER) not yet supported"),
        ),
        // Lexing.
        ("nul_in_sql", "SELECT 'a\0b'".into(), Rows("a\0b")),
        ("unterminated", "SELECT 'abc".into(), Error("unterminated string literal")),
        ("bad_bytes", "SELECT \x01\x02 FROM".into(), Error("unexpected character")),
        ("param_huge", "SELECT ?9223372036854775807".into(), Error("parameter number out of range: 9223372036854775807")),
        ("blob_literal_odd", "SELECT x'abc'".into(), Error("invalid blob literal hex digits")),
        (
            "num_literal_huge",
            format!("SELECT {}, 1e99999, 0x{}", "9".repeat(100_000), "f".repeat(100)),
            Error("invalid hex literal: number too large to fit in target type"),
        ),
        ("ident_huge", format!("SELECT 1 AS \"{}\"", "a".repeat(1_000_000)), Rows("1")),
    ]
}

#[test]
fn hostile_sql_gives_an_answer_or_a_typed_error() {
    let db = TempDb::new("sql");
    let capture = std::env::var_os("DB_CORE_ADVERSARIAL_CAPTURE").is_some();
    let rows = sql_rows();
    assert!(rows.len() >= 62, "the review's 62 probes");
    let mut failures = Vec::new();
    for (name, sql, expect) in rows {
        let (outcome, took) = run(&db.0, sql);
        if capture {
            let shown = match &outcome {
                Result::Ok(rows) => {
                    format!("Rows({:?})", rows.chars().take(120).collect::<String>())
                }
                Err(e) => format!("Error({:?})", e.chars().take(160).collect::<String>()),
            };
            println!("{name:22} {:>6} ms  {shown}", took.as_millis());
            continue;
        }
        if took > TIME_LIMIT {
            failures.push(format!("{name}: took {took:?}"));
        }
        let ok = match (&expect, &outcome) {
            (Expect::Rows(want), Result::Ok(got)) => got == want,
            (Expect::Error(want), Err(got)) => got.contains(want),
            _ => false,
        };
        if !ok {
            failures.push(format!(
                "{name}: got {:?}",
                outcome
                    .map_err(|e| e.chars().take(200).collect::<String>())
                    .map(|r| r.chars().take(200).collect::<String>())
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

const PAGE: usize = 4096;

/// A corrupt or hostile file: how to damage a copy of the fixture (two
/// 4 KiB pages, table `t` rooted at page 2 holding one row), the statement
/// to run on it, and what it must do.
struct FileCase {
    name: &'static str,
    damage: fn(&Path),
    sql: &'static str,
    expect: Expect,
}

fn patch(db: &Path, offset: usize, bytes: &[u8]) {
    let mut data = std::fs::read(db).unwrap();
    data[offset..offset + bytes.len()].copy_from_slice(bytes);
    std::fs::write(db, data).unwrap();
}

/// `db`'s companion file `<db><suffix>`.
fn companion(db: &Path, suffix: &str) -> PathBuf {
    let mut p = db.as_os_str().to_owned();
    p.push(suffix);
    PathBuf::from(p)
}

fn file_cases() -> Vec<FileCase> {
    use Expect::*;
    vec![
        // #872: page 2 made an interior page whose right child is itself.
        FileCase {
            name: "cyclic_interior_page",
            damage: |db| {
                let mut header = [0u8; 12];
                header[0] = 0x05;
                header[5..7].copy_from_slice(&4096u16.to_be_bytes());
                header[8..12].copy_from_slice(&2u32.to_be_bytes());
                patch(db, PAGE, &header);
            },
            sql: "SELECT count(*) FROM t",
            expect: Error("more than 20 levels deep (corrupt or cyclic tree)"),
        },
        // #873: the root page's type byte zeroed.
        FileCase {
            name: "bad_root_page_type",
            damage: |db| patch(db, PAGE, &[0]),
            sql: "SELECT a FROM t",
            expect: Error("unexpected b-tree page type 0x0"),
        },
        // #874: a freelist trunk past the end of the file.
        FileCase {
            name: "freelist_trunk_out_of_range",
            damage: |db| {
                patch(db, 32, &9_999u32.to_be_bytes());
                patch(db, 36, &1u32.to_be_bytes());
            },
            sql: "INSERT INTO t VALUES (2, zeroblob(10000))",
            expect: Error("freelist names page 9999, outside 2..=2"),
        },
        // #874: a freelist trunk that is the live table root.
        FileCase {
            name: "freelist_aliases_a_live_page",
            damage: |db| {
                patch(db, 32, &2u32.to_be_bytes());
                patch(db, 36, &1u32.to_be_bytes());
            },
            sql: "INSERT INTO t VALUES (2, zeroblob(10000))",
            expect: Error("database disk image is malformed"),
        },
        // #875: a sparse 1 GiB -wal with a bad magic: no frames, read in
        // O(header), not buffered whole.
        FileCase {
            name: "huge_bad_magic_wal",
            damage: |db| {
                let wal = std::fs::File::create(companion(db, "-wal")).unwrap();
                wal.set_len(1 << 30).unwrap();
            },
            sql: "SELECT count(*) FROM t",
            expect: Rows("1"),
        },
        // #875: a hot journal whose only record is cut short is a torn
        // tail: nothing to play back, the database as it was.
        FileCase {
            name: "hot_journal_torn_tail",
            damage: |db| {
                let mut journal = vec![0u8; 512 + 100];
                journal[..8].copy_from_slice(&[0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7]);
                journal[8..12].copy_from_slice(&5u32.to_be_bytes()); // records
                journal[16..20].copy_from_slice(&2u32.to_be_bytes()); // pages before
                journal[20..24].copy_from_slice(&512u32.to_be_bytes()); // sector
                journal[24..28].copy_from_slice(&4096u32.to_be_bytes()); // page size
                std::fs::write(companion(db, "-journal"), journal).unwrap();
            },
            sql: "SELECT count(*) FROM t",
            expect: Rows("1"),
        },
    ]
}

#[test]
fn corrupt_and_hostile_files_give_an_answer_or_a_typed_error() {
    let capture = std::env::var_os("DB_CORE_ADVERSARIAL_CAPTURE").is_some();
    let mut failures = Vec::new();
    for case in file_cases() {
        let db = TempDb::new(case.name);
        (case.damage)(&db.0);
        let (outcome, took) = run(&db.0, case.sql.to_string());
        if capture {
            println!("{:30} {:>6} ms  {outcome:?}", case.name, took.as_millis());
            continue;
        }
        if took > TIME_LIMIT {
            failures.push(format!("{}: took {took:?}", case.name));
        }
        let ok = match (&case.expect, &outcome) {
            (Expect::Rows(want), Result::Ok(got)) => got == want,
            (Expect::Error(want), Err(got)) => got.contains(want),
            _ => false,
        };
        if !ok {
            failures.push(format!("{}: got {outcome:?}", case.name));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
