// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Enforces ADR 0000 §Invariants (c): the row (SQLite) side of db-core and
//! the batch/column/stream side never name each other. Ported from
//! sqlite-rs's `tests/unit/layer_isolation.rs`.
//!
//! Why a source scan and not the feature graph: `make check-sqlite-profile`
//! proves that the SQLite *profile* compiles no batch/column/stream file --
//! but a `use crate::vm::batch::Value` in `vm/row/vm.rs` would fail that
//! build outright and be "fixed" by adding `vm-batch` to `vm-row`'s
//! implications, which is precisely how `codegen-row` came to imply
//! `vm-batch` for months (#324). This test names the rule at the module
//! level, so the fix that presents itself is the right one.
//!
//! Shared leaves both sides may use: `value`, `schema`, `types`, `coerce`,
//! `compare`, `functions`, `parser` (the batch grammar is an adapter over the
//! row grammar, #57), `vm::join`, and `engine.rs` itself (the seam, ADR 0017,
//! which converts both `Value` models into `Cell`). None of those are under
//! either side's roots, so they are not scanned.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::arithmetic_side_effects
)]

use std::path::{Path, PathBuf};

/// The SQLite side: everything the SQLite profile is made of.
const ROW_ROOTS: &[&str] = &[
    "src/parser/row.rs",
    "src/vm/row.rs",
    "src/codegen/row.rs",
    "src/storage/row.rs",
    "src/engine/row.rs",
];
const ROW_MAY_NOT_NAME: &[&str] = &[
    "vm::batch",
    "vm::engine",
    "vm::stream",
    "codegen::batch",
    "storage::column",
    "storage::stream",
    "engine::column",
    "engine::stream",
];

/// The other modes.
const OTHER_ROOTS: &[&str] = &[
    "src/vm/batch.rs",
    "src/vm/engine.rs",
    "src/vm/stream.rs",
    "src/codegen/batch_planner.rs",
    "src/storage/column.rs",
    "src/storage/stream.rs",
    "src/engine/column.rs",
];
const OTHER_MAY_NOT_NAME: &[&str] = &["vm::row", "codegen::row", "storage::row", "engine::row"];

fn collect_rs_files(root: &Path, out: &mut Vec<PathBuf>) {
    if root.is_file() {
        out.push(root.to_path_buf());
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// Every `.rs` file under each root, plus each root's sibling submodule
/// directory of the same stem (`src/vm/row.rs` + `src/vm/row/`).
fn collect_module_trees(roots: &[&str]) -> Vec<PathBuf> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let mut files = Vec::new();
    for root in roots {
        let full_root = Path::new(manifest_dir).join(root);
        collect_rs_files(&full_root, &mut files);
        if let Some(stem) = full_root.file_stem() {
            let sibling_dir = full_root.with_file_name(stem);
            if sibling_dir.is_dir() {
                collect_rs_files(&sibling_dir, &mut files);
            }
        }
    }
    files
}

/// Violations as `file:line: <pattern>`. Comment lines are skipped: a doc
/// comment may legitimately *mention* the other side; code may not name it.
fn find_violations(files: &[PathBuf], forbidden: &[&str]) -> Vec<String> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let mut violations = Vec::new();
    for file in files {
        let src = std::fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("reading {}: {e}", file.display()));
        for (i, line) in src.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            for pat in forbidden {
                if line.contains(pat) {
                    let rel = file.strip_prefix(manifest_dir).unwrap_or(file);
                    violations.push(format!("{}:{}: names `{pat}`", rel.display(), i + 1));
                }
            }
        }
    }
    violations
}

#[test]
fn the_sqlite_side_never_names_batch_column_or_stream() {
    let files = collect_module_trees(ROW_ROOTS);
    assert!(
        files.len() > 20,
        "expected the row module trees, found {} files -- check ROW_ROOTS",
        files.len()
    );
    let violations = find_violations(&files, ROW_MAY_NOT_NAME);
    assert!(
        violations.is_empty(),
        "ADR 0000 §(c) violated -- the SQLite side names another execution mode. \
         The fix is never a feature implication (that is how `codegen-row` came to \
         compile `vm/batch.rs` into every sqlite-rs binary); move the shared item to a \
         leaf module (`value`, `schema`, ...) instead:\n{}",
        violations.join("\n")
    );
}

#[test]
fn batch_column_and_stream_never_name_the_sqlite_side() {
    let files = collect_module_trees(OTHER_ROOTS);
    assert!(
        files.len() > 10,
        "expected the batch/column/stream module trees, found {} files -- check OTHER_ROOTS",
        files.len()
    );
    let violations = find_violations(&files, OTHER_MAY_NOT_NAME);
    assert!(
        violations.is_empty(),
        "ADR 0000 §(c) violated -- another execution mode names the SQLite side:\n{}",
        violations.join("\n")
    );
}
