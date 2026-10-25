// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0

//! Child process for `tests/table_reader.rs` (#572): opens the table
//! directory given as the only argument and appends `{"i":<n>,...}` for
//! n = 0, 1, ... forever, each `Ack::Durable`, sealing a page every 37
//! records and flushing every 150. After each ack it prints the record's
//! LSN (always n + 1), then waits a millisecond: the table grows at a
//! bounded rate whatever the disk, so a fast disk does not make the
//! parent's reads ever slower (#726). The parent reads the table from its
//! own process meanwhile, and kills this one with SIGKILL.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::arithmetic_side_effects,
    reason = "test helper process, killed mid-run by design"
)]

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use db_core::storage::ingest::{Ack, TableOptions, TableWriter};

fn main() {
    let table = PathBuf::from(std::env::args().nth(1).expect("usage: table_probe <table>"));
    let mut writer = TableWriter::open(&table, TableOptions::default()).expect("open table");
    let mut next = 0u64;
    loop {
        let n = next;
        next += 1;
        let line = format!(r#"{{"i":{n},"level":"info","msg":"record {n}"}}"#);
        let lsn = writer.append(line.as_bytes(), Ack::Durable).unwrap();
        let mut out = std::io::stdout().lock();
        writeln!(out, "{lsn}").unwrap();
        out.flush().unwrap();
        std::thread::sleep(Duration::from_millis(1));
        if n % 37 == 36 {
            writer.seal_head().unwrap();
        }
        if n % 150 == 149 {
            writer.flush().unwrap();
        }
    }
}
