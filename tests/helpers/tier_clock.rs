// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0

//! The table options and fast clock `tests/helpers/tier_probe.rs` and
//! `tests/tier_crash.rs` share (#648): simulated time is
//! `BASE + (real now - origin) * SPEED`, so every process that knows the
//! origin agrees on it and it never goes backwards across restarts.

use std::sync::atomic::{AtomicI64, Ordering};

use db_core::storage::ingest::{Layout, TableOptions, TierOptions};
use db_core::storage::parquet::schema::{ColumnSpec, ColumnType};

/// Simulated nanoseconds per real nanosecond: an hour every 120 ms, a day
/// every 2.9 s.
pub const SPEED: i64 = 30_000;
/// Rows the open hour holds in sealed pages before it spills.
pub const SPILL_ROWS: usize = 20;
/// 2026-10-11T00:00:00Z.
const BASE: i64 = 20_737 * 86_400 * 1_000_000_000;

static ORIGIN: AtomicI64 = AtomicI64::new(0);

pub fn set_origin(real_ns: i64) {
    ORIGIN.store(real_ns, Ordering::Relaxed);
}

pub fn real_now() -> i64 {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(since.as_nanos()).unwrap_or(i64::MAX)
}

pub fn clock() -> i64 {
    BASE.saturating_add(
        real_now()
            .saturating_sub(ORIGIN.load(Ordering::Relaxed))
            .saturating_mul(SPEED),
    )
}

pub fn options() -> TableOptions {
    TableOptions {
        schema: Some(vec![
            ColumnSpec::new("id", ColumnType::Int64, false),
            ColumnSpec::new("kind", ColumnType::Utf8, false),
            ColumnSpec::new("n", ColumnType::Int64, false),
        ]),
        layout: Layout::Tiered(TierOptions {
            sort_key: vec!["kind".to_string()],
            row_group_max_rows: 16,
            spill_rows: SPILL_ROWS,
            // Replaced files go after a simulated minute.
            delete_grace_ns: 60_000_000_000,
            ..TierOptions::default()
        }),
        clock,
        ..TableOptions::default()
    }
}
