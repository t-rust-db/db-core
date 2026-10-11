// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0

//! Child process for `tests/wal_crash.rs` (#571): opens the WAL in the
//! table directory given as the only argument and appends forever from
//! four threads, mixing `Ack::Durable` and `Ack::Memory` appends and the
//! occasional `roll`, with small segment files. After every `Durable`
//! ack it prints `<lsn>\t<payload>`; the parent kills it with SIGKILL and
//! checks every printed record survived recovery intact.
//!
//! Payloads are `<thread>:<n>:` followed by `n % 300` copies of the
//! thread's letter, so a recovered record is checkable on its own.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    reason = "test helper process, killed mid-run by design"
)]

use std::io::Write;
use std::path::PathBuf;

use db_core::storage::ingest::{Ack, Wal, WalOptions};

fn main() {
    let table = PathBuf::from(std::env::args().nth(1).expect("usage: wal_probe <table>"));
    let options = WalOptions {
        segment_bytes: 64 * 1024,
    };
    let (wal, _) = Wal::open(&table, 0, options).expect("open wal");
    std::thread::scope(|s| {
        for t in 0..4u8 {
            let wal = &wal;
            s.spawn(move || {
                let letter = char::from(b'a' + t);
                for n in 0u64.. {
                    let mut payload = format!("{t}:{n}:");
                    payload.extend(std::iter::repeat_n(letter, (n % 300) as usize));
                    if n % 5 == 4 {
                        wal.append(payload.as_bytes(), Ack::Memory).unwrap();
                        continue;
                    }
                    let lsn = wal.append(payload.as_bytes(), Ack::Durable).unwrap();
                    let mut out = std::io::stdout().lock();
                    writeln!(out, "{lsn}\t{payload}").unwrap();
                    out.flush().unwrap();
                    if n % 97 == 0 {
                        wal.roll().unwrap();
                    }
                }
            });
        }
    });
}
