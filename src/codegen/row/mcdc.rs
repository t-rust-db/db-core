// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! MC/DC vectors for the sqlite-rs codegen moved in by db-core#219.
//!
//! db-core's `make test-mcdc` scans all of `src/` (sqlite-rs scans a
//! curated list that never included its codegen), so every multi-leaf
//! decision in the moved tree needs tagged `mcdc__<id>__vN_*` tests
//! here. They live in this separate, test-only module -- not inside the
//! moved files -- so those files stay byte-comparable with sqlite-rs's
//! tree (only module paths differ; see ADR 0013's drift check).
//! Obligation ids are `<file-stem>_<line>`, so a moved file's later
//! re-sync that shifts lines means re-tagging here, nothing else.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    non_snake_case
)]

mod aggregate_joins;
mod misc;
mod scans;
mod subquery_stmt_eqp;
