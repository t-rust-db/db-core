// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Planner statistics: the pure cost model (`Stats`, `PlanCost`,
//! `estimate_*`, `is_*_worthwhile`) is re-exported from
//! `crate::codegen::row::planner` (moved with the codegen, db-core#219 /
//! t-rust-db/sqlite-rs#19); only [`load_stats`], which reads `sqlite_stat1`
//! through this crate's storage stack, stays here — db-core never sees a
//! page source (db-core ADR 0008).

use crate::schema::TableSchema;
use crate::storage::row::btree::TableCursor;
use crate::storage::row::header::DatabaseHeader;
use crate::storage::row::record::{decode_record, Value};
use crate::storage::row::vfs::PageSource;
use std::collections::HashMap;

pub use crate::codegen::row::planner::{
    estimate_index_cost, estimate_scan_cost, is_automatic_index_worthwhile,
    is_skip_scan_worthwhile, PlanCost, Stats,
};

/// Reads every table's `sqlite_stat1` rows in one pass and returns a
/// `table name -> Stats` map — empty if `sqlite_stat1` isn't in
/// `schemas` at all (no `ANALYZE` has ever run against this database),
/// which is exactly the "behave as before this module existed" case
/// [`estimate_scan_cost`]/[`estimate_index_cost`] already handle safely.
/// Malformed rows are skipped the same way [`Stats::from_stat1_rows`]
/// skips malformed `stat` text — a corrupt `sqlite_stat1` degrades to
/// "no stats for that entry", never a hard error.
pub fn load_stats<P: PageSource>(
    source: P,
    header: &DatabaseHeader,
    schemas: &[TableSchema],
) -> HashMap<String, Stats> {
    let Some(stat1) = schemas
        .iter()
        .find(|s| s.name.eq_ignore_ascii_case("sqlite_stat1"))
    else {
        return HashMap::new();
    };

    let mut rows_by_table: HashMap<String, Vec<(Option<String>, String)>> = HashMap::new();
    let mut cursor = TableCursor::new(source, header, stat1.root_page);
    let Ok(mut row) = cursor.first_row() else {
        return HashMap::new();
    };
    while let Some(r) = row {
        if let Ok(values) = decode_record(&r.payload, header.text_encoding) {
            let tbl = match values.first() {
                Some(Value::Text(s)) => Some(s.to_string()),
                _ => None,
            };
            if let Some(tbl) = tbl {
                let idx = match values.get(1) {
                    Some(Value::Text(s)) => Some(s.to_string()),
                    _ => None,
                };
                let stat = match values.get(2) {
                    Some(Value::Text(s)) => s.to_string(),
                    _ => String::new(),
                };
                rows_by_table.entry(tbl).or_default().push((idx, stat));
            }
        }
        row = match cursor.next_row() {
            Ok(r) => r,
            Err(_) => break,
        };
    }

    rows_by_table
        .into_iter()
        .map(|(tbl, rows)| (tbl, Stats::from_stat1_rows(rows)))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn load_stats_is_empty_when_sqlite_stat1_does_not_exist() {
        let (vfs, header) = crate::storage::row::btree::test_minimal_db(512);
        let pager =
            crate::storage::row::pager::Pager::open(&vfs, std::path::Path::new("/test.db"), 512)
                .unwrap();
        let schemas: Vec<TableSchema> = Vec::new();
        let stats = load_stats(&pager, &header, &schemas);
        assert!(stats.is_empty());
    }

    #[test]
    fn load_stats_decodes_rows_for_every_table() {
        let (vfs, header) = crate::storage::row::btree::test_minimal_db(512);
        let mut pager =
            crate::storage::row::pager::Pager::open(&vfs, std::path::Path::new("/test.db"), 512)
                .unwrap();
        let stat1_root =
            crate::storage::row::btree::ensure_sqlite_stat1_table(&mut pager, &header).unwrap();
        crate::storage::row::btree::insert_stat1_row(
            &mut pager, &header, stat1_root, "t", None, "10000",
        )
        .unwrap();
        crate::storage::row::btree::insert_stat1_row(
            &mut pager,
            &header,
            stat1_root,
            "t",
            Some("idx_a"),
            "10000 10",
        )
        .unwrap();

        let schemas = vec![TableSchema {
            name: "sqlite_stat1".to_string(),
            root_page: stat1_root,
            columns: vec!["tbl".to_string(), "idx".to_string(), "stat".to_string()],
            column_types: vec![String::new(), String::new(), String::new()],
            column_collations: vec![],
            without_rowid: false,
            strict: false,
            is_virtual: false,
            sql: "CREATE TABLE sqlite_stat1(tbl,idx,stat)".to_string(),
            indexes: vec![],
            rowid_alias: None,
        }
        .with_computed_rowid_alias()];

        let all_stats = load_stats(&pager, &header, &schemas);
        let stats = all_stats.get("t").unwrap();
        assert_eq!(stats.table_rows(), Some(10000));
        assert_eq!(stats.index_stats("idx_a"), Some((10000, 10)));
    }

    /// spec 011/Req 3 scenario "Missing stats fall back to a conservative
    /// default".
    #[test]
    fn missing_stats_fall_back_to_max_cost() {
        let stats = Stats::default();
        let cost = estimate_scan_cost(&stats);
        assert_eq!(cost.estimated_rows, u64::MAX);
        assert_eq!(cost.estimated_io, u64::MAX);

        let idx_cost = estimate_index_cost("idx_a", &stats);
        assert_eq!(idx_cost.estimated_rows, u64::MAX);
    }

    /// spec 011/Req 3 scenario "An indexed equality is cheaper than a
    /// scan once stats exist".
    #[test]
    fn indexed_equality_cheaper_than_scan_with_stats() {
        let stats = Stats::from_stat1_rows(vec![
            (None, "10000".to_string()),
            (Some("idx_a".to_string()), "10000 10".to_string()),
        ]);

        let scan = estimate_scan_cost(&stats);
        let indexed = estimate_index_cost("idx_a", &stats);

        assert_eq!(scan.estimated_rows, 10000);
        assert_eq!(indexed.estimated_rows, 10);
        assert!(indexed.estimated_rows < scan.estimated_rows);
    }

    #[test]
    fn unknown_index_name_falls_back_to_unknown() {
        let stats = Stats::from_stat1_rows(vec![(None, "5".to_string())]);
        let cost = estimate_index_cost("no_such_index", &stats);
        assert_eq!(cost.estimated_rows, u64::MAX);
    }

    #[test]
    fn malformed_stat_text_is_skipped_not_a_hard_error() {
        let stats = Stats::from_stat1_rows(vec![(None, "not-a-number".to_string())]);
        assert_eq!(stats.table_rows(), None);
    }

    /// #485: mirrors oracle sqlite3 3.51.0's empirically-confirmed
    /// skip-scan threshold — a leading-column `avg_eq` of 19 picks
    /// skip-scan, 17 does not (`SKIP_SCAN_MIN_AVG_EQ = 18`).
    #[test]
    fn skip_scan_worthwhile_matches_oracle_threshold() {
        let above = Stats::from_stat1_rows(vec![(Some("idx".to_string()), "20001 19".to_string())]);
        assert!(is_skip_scan_worthwhile("idx", &above));

        let below = Stats::from_stat1_rows(vec![(Some("idx".to_string()), "20001 17".to_string())]);
        assert!(!is_skip_scan_worthwhile("idx", &below));

        let at_threshold =
            Stats::from_stat1_rows(vec![(Some("idx".to_string()), "20001 18".to_string())]);
        assert!(is_skip_scan_worthwhile("idx", &at_threshold));
    }

    /// #485: without `ANALYZE` having ever recorded stats for the
    /// index, skip-scan is never chosen — matches oracle's behavior of
    /// never picking skip-scan absent `ANALYZE` history.
    #[test]
    fn skip_scan_never_worthwhile_without_analyze_stats() {
        let stats = Stats::default();
        assert!(!is_skip_scan_worthwhile("idx", &stats));
    }

    /// #545: a table below the row-count threshold isn't worth building
    /// a transient automatic index for.
    #[test]
    fn automatic_index_not_worthwhile_below_threshold() {
        let stats = Stats::from_stat1_rows(vec![(None, "24".to_string())]);
        assert!(!is_automatic_index_worthwhile(&stats));
    }

    /// #545: at/above the row-count threshold, a transient automatic
    /// index is judged worthwhile.
    #[test]
    fn automatic_index_worthwhile_at_and_above_threshold() {
        let stats = Stats::from_stat1_rows(vec![(None, "25".to_string())]);
        assert!(is_automatic_index_worthwhile(&stats));

        let stats = Stats::from_stat1_rows(vec![(None, "10000".to_string())]);
        assert!(is_automatic_index_worthwhile(&stats));
    }

    /// #545: without `ANALYZE` having ever recorded a row count, the
    /// automatic index is never chosen — same "no stats, no
    /// optimization" default as [`is_skip_scan_worthwhile`].
    #[test]
    fn automatic_index_never_worthwhile_without_analyze_stats() {
        let stats = Stats::default();
        assert!(!is_automatic_index_worthwhile(&stats));
    }
}
