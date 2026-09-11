// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Row→batch adapter (ADR-0019, #314): presents one table of an open
//! [`row::RowEngine`](super::row::RowEngine) as a `vm::batch::Batch`/
//! `Segment`/`Source` — the lookup side of a cross-mode join. Mirrors
//! `storage::stream::adapter`'s shape (ADR-0018): whole-table
//! materialization, once per query, of only the columns a program loads
//! (`Program::columns_to_load()`).
//!
//! This module is deliberately **not** part of `engine::row`'s tree: ADR
//! 0000 §(c) forbids the SQLite side from naming `vm::batch` even behind a
//! feature gate (`tests/unit/layer_isolation_test.rs` enforces this by
//! source scan, not just the feature graph — see that test's doc for why).
//! `engine::cross_mode` sits beside `row`/`column`/`stream` as a second
//! seam, alongside `engine.rs` itself, allowed to know both a row engine
//! and batch values; `RowEngine::table_schema`/`with_storage` are the only
//! (mode-agnostic) surface it uses from `engine::row`.
//!
//! The row VM never runs here — this reads the b-tree directly, the same
//! storage layer `RowEngine::catalog()` uses for `sqlite_master`, and
//! decodes each row's record payload. The only new piece is the value
//! crossing: `value::Value` (row) has no `vm::batch::Value` equivalent
//! today, so [`to_batch_value`] is this adapter's own conversion,
//! parallel to (but distinct from) `engine::Cell`'s conversions — ADR
//! 0010/0014 stay intact, the row VM's value type never changes.
//!
//! A `BLOB` column has no `vm::batch::Value` representation (that enum has
//! no `Blob` variant); requesting one is a load error, not silent
//! stringification or truncation.

use std::sync::Arc;

use super::row::RowEngine;
use super::{EngineError, ErrorKind};
use crate::schema::TableSchema;
use crate::storage::row::btree::TableCursor;
use crate::storage::row::header::DatabaseHeader;
use crate::storage::row::pager::Pager;
use crate::storage::row::record::decode_record;
use crate::value::Value as RowValue;
use crate::vm::batch::{Batch, Segment as VmSegment, Source as VmSource, Value, VmError};

/// A whole-table snapshot of `table`'s `columns` from `engine`, as a
/// `vm::batch::Batch` -- the lookup side of a cross-mode join. The row VM
/// never runs; this reads the b-tree directly through `engine`'s own
/// pager, the same storage layer its schema catalog uses.
pub fn scan_table_as_batch(
    engine: &RowEngine,
    table: &str,
    columns: &[String],
) -> Result<Batch, EngineError> {
    let schema = engine.table_schema(table)?;
    engine
        .with_storage(|pager, header| materialize_table(pager, header, &schema, columns))
        .map_err(|e| EngineError::new(ErrorKind::Execute, e.to_string()))
}

/// `value::Value` (row) → `vm::batch::Value` (batch), the adapter's own
/// boundary conversion. `Blob` has no batch equivalent and is an error, not
/// a silent cast — cross-mode joins key on text/integer dimension columns,
/// never on blobs.
fn to_batch_value(v: RowValue) -> Result<Value, VmError> {
    Ok(match v {
        RowValue::Null => Value::Null,
        RowValue::Integer(n) => Value::Int(n),
        RowValue::Real(x) => Value::Float(x),
        RowValue::Text(s) => Value::Str(s.to_string().into()),
        RowValue::Blob(_) => {
            return Err(VmError::SegmentLoad {
                reason: "BLOB columns have no vm::batch::Value representation".to_string(),
            })
        }
    })
}

/// Reads every row of `schema`'s table through `pager`, materializing only
/// `columns` (in request order) into a [`Batch`]. Whole-table snapshot,
/// once: v1 per ADR-0019 (dimension tables are the assumed shape; a
/// key-restricted scan is future work if a lookup table's size makes this
/// scan measurably worse than one seek per key).
pub fn materialize_table(
    pager: &Pager,
    header: &DatabaseHeader,
    schema: &TableSchema,
    columns: &[String],
) -> Result<Batch, VmError> {
    let col_indices: Vec<Option<usize>> = columns
        .iter()
        .map(|name| schema.columns.iter().position(|c| c == name))
        .collect();
    for (name, idx) in columns.iter().zip(&col_indices) {
        if idx.is_none() {
            return Err(VmError::SegmentLoad {
                reason: format!("unknown column `{name}` on table `{}`", schema.name),
            });
        }
    }

    let mut columns_out: Vec<Vec<Value>> = vec![Vec::new(); columns.len()];
    let mut num_rows = 0usize;

    if schema.root_page != 0 {
        let mut cursor = TableCursor::new(pager, header, schema.root_page);
        let mut next = cursor.first_row().map_err(|e| VmError::SegmentLoad {
            reason: format!("table `{}`: {e}", schema.name),
        })?;
        while let Some(row) = next {
            let mut values = decode_record(&row.payload, header.text_encoding).map_err(|e| {
                VmError::SegmentLoad {
                    reason: format!("table `{}` rowid {}: {e}", schema.name, row.rowid),
                }
            })?;
            values.resize(schema.columns.len(), RowValue::Null);
            if let Some(alias) = schema.rowid_alias {
                if let Some(slot @ RowValue::Null) = values.get_mut(alias) {
                    *slot = RowValue::Integer(row.rowid);
                }
            }
            for (out, idx) in columns_out.iter_mut().zip(&col_indices) {
                let value =
                    idx.and_then(|i| values.get(i))
                        .ok_or_else(|| VmError::SegmentLoad {
                            reason: format!(
                                "table `{}` rowid {}: column index out of range",
                                schema.name, row.rowid
                            ),
                        })?;
                out.push(to_batch_value(value.clone())?);
            }
            num_rows = num_rows.saturating_add(1);
            next = cursor.next_row().map_err(|e| VmError::SegmentLoad {
                reason: format!("table `{}`: {e}", schema.name),
            })?;
        }
    }

    let mut batch = Batch::new(num_rows);
    for (name, values) in columns.iter().zip(columns_out) {
        batch = batch.with_column(name.clone(), values);
    }
    Ok(batch)
}

/// A materialized table snapshot as a `vm::batch::Segment`: `load()`
/// returns the same `Arc<Batch>` every time, computed once at construction.
#[derive(Debug, Clone)]
pub struct RowTableSegment {
    batch: Arc<Batch>,
}

impl RowTableSegment {
    /// Wrap an already-materialized `batch` (see [`materialize_table`]).
    #[must_use]
    pub fn new(batch: Batch) -> Self {
        Self {
            batch: Arc::new(batch),
        }
    }
}

impl VmSegment for RowTableSegment {
    fn load(&self) -> Result<Arc<Batch>, VmError> {
        Ok(Arc::clone(&self.batch))
    }
}

/// A materialized table snapshot as a `vm::batch::Source`: yields the whole
/// table as one `Batch` on the first `next_batch()` call, `None`
/// thereafter — the lookup side is bounded and read once per query.
#[derive(Debug, Default)]
pub struct RowTableSource {
    batch: Option<Batch>,
}

impl RowTableSource {
    /// Wrap an already-materialized `batch` (see [`materialize_table`]).
    #[must_use]
    pub fn new(batch: Batch) -> Self {
        Self { batch: Some(batch) }
    }
}

impl VmSource for RowTableSource {
    fn next_batch(&mut self) -> Option<Batch> {
        self.batch.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::row::RowEngine;
    use crate::engine::Engine;
    use crate::vm::batch::{Instruction, JoinKind, Opcode as BatchOpcode, Program};
    use crate::vm::engine::{run_join, JoinProgram};

    const FIXTURE: &str = "tests/corpus/fixtures/btrees/table_single_page.db";

    struct TempDb(std::path::PathBuf);

    impl TempDb {
        fn new(label: &str) -> Self {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "db-core-batch-adapter-{label}-{}-{}.db",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .subsec_nanos()
            ));
            std::fs::copy(FIXTURE, &path).expect("copy fixture");
            TempDb(path)
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            std::fs::remove_file(&self.0).ok();
            std::fs::remove_file(format!("{}-journal", self.0.display())).ok();
        }
    }

    fn hosts_engine(label: &str) -> (TempDb, RowEngine) {
        let db = TempDb::new(label);
        let mut engine = RowEngine::open(&db.0).unwrap();
        engine
            .run_query(
                "CREATE TABLE hosts(id INTEGER PRIMARY KEY, name TEXT, region TEXT);\
                 INSERT INTO hosts VALUES (1, 'web01', 'eu');\
                 INSERT INTO hosts VALUES (2, 'web02', NULL);\
                 INSERT INTO hosts VALUES (3, NULL, 'us');",
            )
            .unwrap();
        (db, engine)
    }

    #[test]
    fn to_batch_value_converts_every_row_variant() {
        assert_eq!(to_batch_value(RowValue::Null).unwrap(), Value::Null);
        assert_eq!(to_batch_value(RowValue::Integer(7)).unwrap(), Value::Int(7));
        assert_eq!(
            to_batch_value(RowValue::Real(1.5)).unwrap(),
            Value::Float(1.5)
        );
        assert_eq!(
            to_batch_value(RowValue::Text("hi".into())).unwrap(),
            Value::Str("hi".into())
        );
    }

    #[test]
    fn to_batch_value_rejects_blob() {
        let err = to_batch_value(RowValue::Blob(vec![1, 2, 3].into())).unwrap_err();
        assert!(matches!(err, VmError::SegmentLoad { .. }));
    }

    #[test]
    fn scan_table_as_batch_materializes_whole_table_with_rowid_alias() {
        let (_db, engine) = hosts_engine("scan");
        let batch =
            scan_table_as_batch(&engine, "hosts", &["id".to_string(), "name".to_string()]).unwrap();
        assert_eq!(batch.num_rows, 3);
        assert_eq!(
            batch.columns["id"].as_slice(),
            &[Value::Int(1), Value::Int(2), Value::Int(3)]
        );
        assert_eq!(batch.columns["name"][1], Value::Str("web02".into()));
        assert_eq!(batch.columns["name"][2], Value::Null);
    }

    #[test]
    fn scan_table_as_batch_rejects_unknown_column() {
        let (_db, engine) = hosts_engine("unknown-col");
        let err = scan_table_as_batch(&engine, "hosts", &["nope".to_string()]).unwrap_err();
        assert!(err.to_string().contains("nope"));
    }

    #[test]
    fn row_table_segment_and_source_yield_the_same_batch() {
        let (_db, engine) = hosts_engine("segment-source");
        let batch = scan_table_as_batch(&engine, "hosts", &["id".to_string()]).unwrap();
        let seg = RowTableSegment::new(batch.clone());
        assert_eq!(seg.load().unwrap().num_rows, 3);

        let mut src = RowTableSource::new(batch);
        let first = src.next_batch().expect("first call yields the snapshot");
        assert_eq!(first.num_rows, 3);
        assert!(src.next_batch().is_none(), "bounded lookup side, read once");
    }

    /// End-to-end: the SQLite lookup side (this adapter's `materialize_table`)
    /// as the build/right side of `HashBuild`/`HashProbe`, an in-memory
    /// driving `Batch` as the probe/left side -- exactly the join
    /// `codegen::batch::compile_join` already compiles (`run_join` always
    /// takes `right: &Batch`), with the right side now read through
    /// `engine::row` instead of a literal `Batch`. A LEFT join so an
    /// unmatched driving key (`db01`, no such host) and a NULL lookup key
    /// (`web02`'s row has `region = NULL`) both show up as `NULL`, never a
    /// silent match.
    #[test]
    fn hash_join_against_a_sqlite_lookup_table() {
        let (_db, engine) = hosts_engine("join");
        let lookup = scan_table_as_batch(
            &engine,
            "hosts",
            &["name".to_string(), "region".to_string()],
        )
        .unwrap();

        let driving = Batch::new(3).with_column(
            "host".to_string(),
            vec![
                Value::Str("web01".into()),
                Value::Str("web02".into()),
                Value::Str("db01".into()),
            ],
        );

        let build = Program::new(vec![
            Instruction::new(BatchOpcode::LoadColumn {
                reg: 0,
                column: "name".into(),
            }),
            Instruction::new(BatchOpcode::LoadColumn {
                reg: 1,
                column: "region".into(),
            }),
            Instruction::new(BatchOpcode::HashBuild {
                key_cols: vec![0].into(),
                payload_cols: vec![1].into(),
                table: 0,
            }),
            Instruction::new(BatchOpcode::Halt),
        ]);
        let probe = Program::new(vec![
            Instruction::new(BatchOpcode::LoadColumn {
                reg: 0,
                column: "host".into(),
            }),
            Instruction::new(BatchOpcode::HashProbe {
                key_cols: vec![0].into(),
                table: 0,
                payload_dst: vec![1].into(),
                kind: JoinKind::Left,
            }),
            Instruction::new(BatchOpcode::Halt),
        ]);
        let body = Program::new(vec![
            Instruction::new(BatchOpcode::LoadColumn {
                reg: 0,
                column: "host".into(),
            }),
            Instruction::new(BatchOpcode::LoadColumn {
                reg: 1,
                column: "region".into(),
            }),
            Instruction::new(BatchOpcode::Emit {
                registers: vec![0, 1].into(),
            }),
            Instruction::new(BatchOpcode::Halt),
        ]);

        let plan = JoinProgram {
            left_columns: vec!["host".to_string()],
            right_columns: vec!["region".to_string()],
            build,
            probe,
            payload_dst: vec![1],
            body,
        };

        let rows = run_join(&driving, &lookup, &plan).unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Value::Str("web01".into()), Value::Str("eu".into())],
                vec![Value::Str("web02".into()), Value::Null],
                vec![Value::Str("db01".into()), Value::Null],
            ]
        );
    }
}
