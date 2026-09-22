//! Rendering db-core's rows the way `sqlite3 .mode quote` renders the
//! oracle's, and the verdict from putting the two side by side.

use std::collections::BTreeMap;

use db_core::engine::{Cell, QueryResult};
use db_core::parser::row::{parse_select, ParseOutcome};
use db_core::value::format_real;

use crate::oracle::OracleOutcome;
use crate::stage::{Outcome, Rejection};

/// Relative tolerance for the float-drift retry.
pub const FLOAT_REL_EPS: f64 = 1e-12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Verdict {
    /// Both ran, rows agree.
    Pass,
    /// Both ran, rows differ.
    WrongAnswer,
    /// Both ran, rows differ only in reals within [`FLOAT_REL_EPS`].
    FloatDrift,
    /// We rejected as invalid or failed to compile; oracle ran it.
    Gap,
    /// We rejected as not-yet-supported; oracle ran it. Expected outside
    /// landed V-blocks, so counted, never a finding.
    Unsupported,
    /// We ran it; oracle rejected it.
    OverPermissive,
    /// Both rejected.
    BothReject,
    /// Oracle exceeded the timeout; ours did not.
    OracleHang,
    /// Ours was a totality finding; oracle not consulted.
    Totality,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::WrongAnswer => "wrong-answer",
            Verdict::FloatDrift => "float-drift",
            Verdict::Gap => "gap",
            Verdict::Unsupported => "unsupported",
            Verdict::OverPermissive => "over-permissive",
            Verdict::BothReject => "both-reject",
            Verdict::OracleHang => "oracle-hang",
            Verdict::Totality => "totality",
        }
    }

    /// Verdicts that become findings.
    pub fn is_finding(self) -> bool {
        matches!(
            self,
            Verdict::WrongAnswer | Verdict::Gap | Verdict::OverPermissive | Verdict::OracleHang
        )
    }
}

/// One statement's comparison, kept alongside its [`Outcome`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparison {
    pub verdict: Verdict,
    /// Our rows, rendered (empty when we did not run it).
    pub ours: Vec<Vec<String>>,
    /// Oracle rows, rendered (empty when it did not run it).
    pub oracle: Vec<Vec<String>>,
    /// Oracle error message, when it rejected.
    pub oracle_message: Option<String>,
    /// Our rejection message, when we rejected.
    pub ours_message: Option<String>,
    /// How rows were compared: `ordered`, `sorted`, or `count-only`.
    pub mode: &'static str,
    /// Statements the engine had applied before this one, plus this one:
    /// a self-contained repro. Filled in by the worker.
    pub script: Vec<String>,
}

pub type VerdictCounts = BTreeMap<Verdict, usize>;

/// `.mode quote` rendering of one cell.
pub fn render_cell(cell: &Cell) -> String {
    match cell {
        Cell::Null => "NULL".to_string(),
        Cell::Int(n) => n.to_string(),
        Cell::Real(x) => format_real(*x),
        Cell::Bool(b) => if *b { "1" } else { "0" }.to_string(),
        Cell::Text(s) => format!("'{}'", s.replace('\'', "''")),
        Cell::Blob(b) => {
            let mut s = String::with_capacity(b.len().saturating_mul(2).saturating_add(3));
            s.push_str("X'");
            for byte in b {
                s.push_str(&format!("{byte:02x}"));
            }
            s.push('\'');
            s
        }
    }
}

pub fn render_rows(result: &QueryResult) -> Vec<Vec<String>> {
    result
        .rows
        .iter()
        .map(|r| r.iter().map(|c| normalize_cell(&render_cell(c))).collect())
        .collect()
}

/// Puts a rendered cell into canonical form: a bare real (not quoted,
/// not a blob, not NULL, has a `.`/exponent) is re-rendered with
/// `format_real` (`%!.15g`). sqlite3's `.mode quote` prints reals at
/// round-trip precision (`3.140000000000000124`), db-core at display
/// precision (`3.14`); both denote the same double.
pub fn normalize_cell(cell: &str) -> String {
    let looks_real = !cell.starts_with('\'')
        && !cell.starts_with("X'")
        && cell != "NULL"
        && (cell.contains('.') || cell.contains('e') || cell.contains('E'));
    if looks_real {
        if let Ok(v) = cell.parse::<f64>() {
            return format_real(v);
        }
    }
    cell.to_string()
}

pub fn normalize_rows(rows: &[Vec<String>]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| r.iter().map(|c| normalize_cell(c)).collect())
        .collect()
}

/// How a statement's result set may legitimately vary between two
/// correct engines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowOrder {
    /// `ORDER BY` present: compare in order.
    Ordered,
    /// No `ORDER BY`: any permutation is correct; compare sorted.
    Unordered,
    /// `LIMIT` without `ORDER BY`: any subset is correct; compare counts.
    ArbitrarySubset,
}

pub fn row_order(sql: &str) -> RowOrder {
    match parse_select(sql) {
        ParseOutcome::Accepted(select) => {
            if !select.order_by.is_empty() {
                RowOrder::Ordered
            } else if select.limit.is_some() {
                RowOrder::ArbitrarySubset
            } else {
                RowOrder::Unordered
            }
        }
        // Not a SELECT (or not parseable as one): whatever rows come back
        // are compared sorted -- PRAGMA and EXPLAIN output has no
        // guaranteed order either.
        _ => RowOrder::Unordered,
    }
}

fn floats_close(a: &str, b: &str) -> bool {
    match (a.parse::<f64>(), b.parse::<f64>()) {
        (Ok(x), Ok(y)) => {
            if x == y {
                return true;
            }
            let scale = x.abs().max(y.abs());
            (x - y).abs() <= FLOAT_REL_EPS * scale
        }
        _ => false,
    }
}

fn rows_equal_within_float_eps(a: &[Vec<String>], b: &[Vec<String>]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(ra, rb)| {
            ra.len() == rb.len()
                && ra
                    .iter()
                    .zip(rb)
                    .all(|(ca, cb)| ca == cb || floats_close(ca, cb))
        })
}

/// Joins our outcome with the oracle's.
pub fn compare(
    sql: &str,
    ours: &Outcome,
    our_rows: Option<&QueryResult>,
    oracle: Option<&OracleOutcome>,
) -> Comparison {
    let mut cmp = Comparison {
        verdict: Verdict::Totality,
        ours: Vec::new(),
        oracle: Vec::new(),
        oracle_message: None,
        ours_message: match ours {
            Outcome::Rejected { message, .. } => Some(message.clone()),
            _ => None,
        },
        mode: "ordered",
        script: Vec::new(),
    };
    let Some(oracle) = oracle else {
        return cmp;
    };
    match (ours, oracle) {
        (Outcome::Panic { .. } | Outcome::Hang { .. } | Outcome::Corrupted { .. }, _) => {
            cmp.verdict = Verdict::Totality;
        }
        (Outcome::Rejected { .. }, OracleOutcome::Error(msg)) => {
            cmp.verdict = Verdict::BothReject;
            cmp.oracle_message = Some(msg.clone());
        }
        (Outcome::Rejected { kind, .. }, OracleOutcome::Rows(rows)) => {
            cmp.oracle = normalize_rows(rows);
            cmp.verdict = match kind {
                Rejection::Unsupported => Verdict::Unsupported,
                Rejection::Invalid | Rejection::Compile | Rejection::Execute => Verdict::Gap,
            };
        }
        (Outcome::Ok, OracleOutcome::Error(msg)) => {
            cmp.ours = our_rows.map(render_rows).unwrap_or_default();
            cmp.oracle_message = Some(msg.clone());
            cmp.verdict = Verdict::OverPermissive;
        }
        (Outcome::Ok, OracleOutcome::Rows(rows)) => {
            let mut ours_r = our_rows.map(render_rows).unwrap_or_default();
            let mut oracle_r = normalize_rows(rows);
            let order = row_order(sql);
            cmp.verdict = match order {
                RowOrder::ArbitrarySubset => {
                    cmp.mode = "count-only";
                    if ours_r.len() == oracle_r.len() {
                        Verdict::Pass
                    } else {
                        Verdict::WrongAnswer
                    }
                }
                RowOrder::Ordered | RowOrder::Unordered => {
                    if order == RowOrder::Unordered {
                        cmp.mode = "sorted";
                        ours_r.sort();
                        oracle_r.sort();
                    }
                    if ours_r == oracle_r {
                        Verdict::Pass
                    } else if rows_equal_within_float_eps(&ours_r, &oracle_r) {
                        Verdict::FloatDrift
                    } else {
                        Verdict::WrongAnswer
                    }
                }
            };
            cmp.ours = ours_r;
            cmp.oracle = oracle_r;
        }
    }
    cmp
}

/// `ours` vs `oracle` rows as a short text block for reports (first
/// differing row, capped).
pub fn render_diff(cmp: &Comparison, max_rows: usize) -> String {
    let mut s = String::new();
    s.push_str(&format!("mode: {}\n", cmp.mode));
    if let Some(m) = &cmp.ours_message {
        s.push_str(&format!("ours: {m}\n"));
    }
    if let Some(m) = &cmp.oracle_message {
        s.push_str(&format!("oracle: {m}\n"));
    }
    s.push_str(&format!(
        "ours ({} rows) | oracle ({} rows)\n",
        cmp.ours.len(),
        cmp.oracle.len()
    ));
    let n = cmp.ours.len().max(cmp.oracle.len()).min(max_rows);
    for i in 0..n {
        let a = cmp.ours.get(i).map(|r| r.join(",")).unwrap_or_default();
        let b = cmp.oracle.get(i).map(|r| r.join(",")).unwrap_or_default();
        let mark = if a == b { " " } else { "!" };
        s.push_str(&format!("{mark} {a} | {b}\n"));
    }
    s
}
