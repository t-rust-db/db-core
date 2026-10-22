// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! [`super::step`]'s schema opcode family (#661, #670).

use super::*;

/// [`step`] for one family: transactions, pragmas and DDL. `step` sends
/// only these opcodes here.
#[inline(always)]
pub(super) fn step_schema(vm: &mut Vm, instr: &Instruction) -> Result<Step, ExecError> {
    match instr.opcode {
        Opcode::Transaction => {
            if !vm.autocommit {
                return Err(ExecError::TransactionAlreadyActive);
            }
            if let Some(hook) = vm.transaction_hook.as_mut() {
                hook.begin(instr.p1).map_err(ExecError::TransactionFailed)?;
            }
            vm.autocommit = false;
            Ok(Step::Next)
        }
        Opcode::AutoCommit => {
            if vm.autocommit {
                return Err(if instr.p2 == 0 {
                    ExecError::NoActiveTransactionToRollback
                } else {
                    ExecError::NoActiveTransactionToCommit
                });
            }
            if let Some(hook) = vm.transaction_hook.as_mut() {
                if instr.p2 == 0 {
                    hook.rollback().map_err(ExecError::TransactionFailed)?;
                } else {
                    hook.commit().map_err(ExecError::TransactionFailed)?;
                }
            }
            vm.autocommit = true;
            Ok(Step::Next)
        }
        Opcode::SetJournalMode => {
            if !vm.autocommit {
                return Err(ExecError::JournalModeChangeDuringTransaction);
            }
            // The attached pager (via the transaction hook) switches its
            // on-disk journal mode; with no hook this is the no-op
            // sqlite-rs itself falls back to for a read-only connection.
            // sqlite3 answers the assignment with the resulting mode, one
            // TEXT row (#561) -- only when a pager applied it.
            if let Some(hook) = vm.transaction_hook.as_mut() {
                hook.set_journal_mode(instr.p1)
                    .map_err(ExecError::TransactionFailed)?;
                let mode = if instr.p1 == JOURNAL_MODE_WAL {
                    "wal"
                } else {
                    "delete"
                };
                vm.emit_row(vec![Value::Text(mode.into())]);
            }
            Ok(Step::Next)
        }
        Opcode::Synchronous => {
            // Query form reports the pager's level (FULL with no pager,
            // sqlite-rs's no-writer fallback); otherwise sets it.
            if instr.p1 == SYNCHRONOUS_QUERY {
                let level = vm
                    .transaction_hook
                    .as_deref()
                    .and_then(|h| h.synchronous())
                    .unwrap_or(SYNCHRONOUS_FULL);
                vm.emit_row(vec![Value::Integer(i64::from(level))]);
                return Ok(Step::Next);
            }
            if !vm.autocommit {
                return Err(ExecError::SynchronousChangeDuringTransaction);
            }
            if let Some(hook) = vm.transaction_hook.as_mut() {
                hook.set_synchronous(instr.p1)
                    .map_err(ExecError::TransactionFailed)?;
            }
            Ok(Step::Next)
        }
        Opcode::IntegrityCheck => {
            // `p1 != 0` is quick_check. One TEXT row per problem line
            // (sqlite-rs `pragma::integrity_check`); without a hook that
            // can read the file this stays Unimplemented.
            let quick = instr.p1 != 0;
            let Some(result) = vm
                .transaction_hook
                .as_mut()
                .and_then(|h| h.integrity_check(quick))
            else {
                return Err(ExecError::Unimplemented {
                    opcode: Opcode::IntegrityCheck,
                });
            };
            for line in result.map_err(ExecError::TransactionFailed)? {
                vm.emit_row(vec![Value::Text(line.into())]);
            }
            Ok(Step::Next)
        }

        Opcode::CreateTable => {
            let P4::CreateTable {
                name,
                sql,
                without_rowid,
                autoindexes,
            } = &instr.p4
            else {
                return Err(ExecError::MalformedInstruction {
                    opcode: "CreateTable",
                    reason: format!("expected a CreateTable P4, got {:?}", instr.p4),
                });
            };
            let (name, sql, without_rowid, autoindexes) = (
                name.clone(),
                sql.clone(),
                *without_rowid,
                autoindexes.clone(),
            );
            let storage = vm.schema_storage("CreateTable")?;
            // A WITHOUT ROWID table is an index b-tree keyed by its
            // PRIMARY KEY.
            let root = if without_rowid {
                storage.create_index_root()
            } else {
                storage.create_table_root()
            }
            .map_err(ExecError::SchemaStorageFailed)?;
            storage
                .insert_master_row("table", &name, &name, root, &sql)
                .map_err(ExecError::SchemaStorageFailed)?;
            // sqlite3's page order: the table, then each implicit index
            // (an empty table needs no population).
            for index_name in &autoindexes {
                let index_root = storage
                    .create_index_root()
                    .map_err(ExecError::SchemaStorageFailed)?;
                storage
                    .insert_autoindex_row(index_name, &name, index_root)
                    .map_err(ExecError::SchemaStorageFailed)?;
            }
            storage
                .bump_schema_cookie()
                .map_err(ExecError::SchemaStorageFailed)?;
            Ok(Step::Next)
        }
        Opcode::CreateView => {
            // sqlite-rs `cursor::create_view`: a view has no b-tree — one
            // sqlite_master row with rootpage 0, then the cookie bump.
            let P4::CreateView { name, sql } = &instr.p4 else {
                return Err(ExecError::MalformedInstruction {
                    opcode: "CreateView",
                    reason: format!("expected a CreateView P4, got {:?}", instr.p4),
                });
            };
            let (name, sql) = (name.clone(), sql.clone());
            let storage = vm.schema_storage("CreateView")?;
            storage
                .insert_master_row("view", &name, &name, 0, &sql)
                .map_err(ExecError::SchemaStorageFailed)?;
            storage
                .bump_schema_cookie()
                .map_err(ExecError::SchemaStorageFailed)?;
            Ok(Step::Next)
        }
        Opcode::DropView => {
            // A view has no b-tree: its sqlite_master row, then the
            // cookie bump (#552).
            let P4::DropView { name } = &instr.p4 else {
                return Err(ExecError::MalformedInstruction {
                    opcode: "DropView",
                    reason: format!("expected a DropView P4, got {:?}", instr.p4),
                });
            };
            let name = name.clone();
            let storage = vm.schema_storage("DropView")?;
            storage
                .delete_master_row("view", &name)
                .map_err(ExecError::SchemaStorageFailed)?;
            storage
                .bump_schema_cookie()
                .map_err(ExecError::SchemaStorageFailed)?;
            Ok(Step::Next)
        }
        Opcode::CreateIndex => {
            let P4::CreateIndex {
                name,
                table_name,
                table_root_page,
                sql,
                column_indices,
                ..
            } = &instr.p4
            else {
                return Err(ExecError::MalformedInstruction {
                    opcode: "CreateIndex",
                    reason: format!("expected a CreateIndex P4, got {:?}", instr.p4),
                });
            };
            let (name, table_name, table_root_page, sql, column_indices) = (
                name.clone(),
                table_name.clone(),
                *table_root_page,
                sql.clone(),
                column_indices.clone(),
            );
            let storage = vm.schema_storage("CreateIndex")?;
            let root = storage
                .create_index_root()
                .map_err(ExecError::SchemaStorageFailed)?;
            storage
                .populate_index(root, table_root_page, &column_indices)
                .map_err(ExecError::SchemaStorageFailed)?;
            storage
                .insert_master_row("index", &name, &table_name, root, &sql)
                .map_err(ExecError::SchemaStorageFailed)?;
            storage
                .bump_schema_cookie()
                .map_err(ExecError::SchemaStorageFailed)?;
            Ok(Step::Next)
        }
        Opcode::DropTable => {
            let P4::DropTable {
                name,
                root_page,
                indexes,
            } = &instr.p4
            else {
                return Err(ExecError::MalformedInstruction {
                    opcode: "DropTable",
                    reason: format!("expected a DropTable P4, got {:?}", instr.p4),
                });
            };
            let (name, root_page, indexes) = (name.clone(), *root_page, indexes.clone());
            let storage = vm.schema_storage("DropTable")?;
            // sqlite-rs order (page-image parity with the oracle): each
            // index's pages then its row, then the table's pages, then
            // its row.
            for (index_name, index_root) in &indexes {
                storage
                    .free_root(*index_root)
                    .map_err(ExecError::SchemaStorageFailed)?;
                storage
                    .delete_master_row("index", index_name)
                    .map_err(ExecError::SchemaStorageFailed)?;
            }
            storage
                .free_root(root_page)
                .map_err(ExecError::SchemaStorageFailed)?;
            storage
                .delete_master_row("table", &name)
                .map_err(ExecError::SchemaStorageFailed)?;
            storage
                .bump_schema_cookie()
                .map_err(ExecError::SchemaStorageFailed)?;
            Ok(Step::Next)
        }
        Opcode::DropIndex => {
            let P4::DropIndex { name, root_page } = &instr.p4 else {
                return Err(ExecError::MalformedInstruction {
                    opcode: "DropIndex",
                    reason: format!("expected a DropIndex P4, got {:?}", instr.p4),
                });
            };
            let (name, root_page) = (name.clone(), *root_page);
            let storage = vm.schema_storage("DropIndex")?;
            storage
                .free_root(root_page)
                .map_err(ExecError::SchemaStorageFailed)?;
            storage
                .delete_master_row("index", &name)
                .map_err(ExecError::SchemaStorageFailed)?;
            storage
                .bump_schema_cookie()
                .map_err(ExecError::SchemaStorageFailed)?;
            Ok(Step::Next)
        }
        Opcode::Analyze => {
            let P4::Analyze { targets } = &instr.p4 else {
                return Err(ExecError::MalformedInstruction {
                    opcode: "Analyze",
                    reason: format!("expected an Analyze P4, got {:?}", instr.p4),
                });
            };
            let targets = targets.clone();
            let storage = vm.schema_storage("Analyze")?;
            for target in &targets {
                storage
                    .write_stat1(target)
                    .map_err(ExecError::SchemaStorageFailed)?;
            }
            Ok(Step::Next)
        }

        other => Err(ExecError::MalformedInstruction {
            opcode: "step",
            reason: format!("{other:?} dispatched to step_schema"),
        }),
    }
}
